//! End-to-end controls must never wake a stopped daemon or discard a draft.
use fwm_core::model::{Config, DesiredState};
use serde_json::Value;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

struct Fixture {
    directory: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let ssh = directory.path().join("ssh_config");
        fs::write(
            &ssh,
            "Host *\n IdentityAgent none\n GlobalKnownHostsFile none\n",
        )
        .unwrap();
        let fixture = Self { directory };
        fixture.ok(&[
            "server",
            "add",
            "dev",
            "--host",
            "127.0.0.1",
            "--port",
            "1",
            "--user",
            "fixture",
            "--ssh-config",
            ssh.to_str().unwrap(),
        ]);
        fixture
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fwm"))
            .arg("--config-dir")
            .arg(self.directory.path())
            .arg("--json")
            .args(args)
            .output()
            .unwrap()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn add(&self, name: &str, port: &str, running: bool) {
        let mut args = vec!["add", name, "--server", "dev", "--local", "--port", port];
        if !running {
            args.push("--disabled");
        }
        self.ok(&args);
    }
    fn config(&self) -> Config {
        serde_json::from_value(self.ok(&["config", "export"])).unwrap()
    }
    fn candidate(&self) -> PathBuf {
        self.directory.path().join("config.toml")
    }
    fn running(&self) -> bool {
        self.ok(&["daemon", "status"])["daemon_running"]
            .as_bool()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.run(&["daemon", "stop"]);
    }
}

#[test]
fn offline_down_and_remove_leave_other_running_intent_offline() {
    let fixture = Fixture::new();
    fixture.add("first", "31001", true);
    fixture.add("other", "31002", true);
    fixture.ok(&["daemon", "stop"]);
    assert!(!fixture.running());
    fixture.ok(&["down", "first"]);
    assert!(!fixture.running(), "down must not start any connection");
    let config = fixture.config();
    assert_eq!(
        config.forward("first").unwrap().desired_state,
        DesiredState::Stopped
    );
    assert_eq!(
        config.forward("other").unwrap().desired_state,
        DesiredState::Running
    );
    fixture.ok(&["remove", "first"]);
    assert!(!fixture.running(), "remove must not start any connection");
    assert!(fixture.config().forward("first").is_none());
    assert_eq!(
        fixture.config().forward("other").unwrap().desired_state,
        DesiredState::Running
    );
}

#[test]
fn stop_and_delete_preserve_draft_bytes_and_survive_later_reload() {
    let fixture = Fixture::new();
    fixture.add("first", "31011", true);
    fixture.add("other", "31012", false);
    let mut draft = fixture.config();
    draft.defaults.retry.max_delay_secs = 61;
    // Even a no-op down must override a pending draft which would enable it.
    draft
        .forwards
        .iter_mut()
        .find(|rule| rule.name == "other")
        .unwrap()
        .desired_state = DesiredState::Running;
    let draft_text = toml::to_string_pretty(&draft).unwrap();
    fs::write(fixture.candidate(), &draft_text).unwrap();
    fixture.ok(&["down", "other"]);
    assert_eq!(fs::read_to_string(fixture.candidate()).unwrap(), draft_text);
    fixture.ok(&["daemon", "stop"]);
    fixture.ok(&["down", "first"]);
    assert!(!fixture.running());
    assert_eq!(fs::read_to_string(fixture.candidate()).unwrap(), draft_text);
    fixture.ok(&["daemon", "start"]);
    fixture.ok(&["config", "reload"]);
    let applied = fixture.config();
    assert_eq!(applied.defaults.retry.max_delay_secs, 61);
    assert!(
        applied
            .forwards
            .iter()
            .all(|rule| rule.desired_state == DesiredState::Stopped)
    );

    fs::write(fixture.candidate(), "incomplete [[draft").unwrap();
    fixture.ok(&["remove", "first"]);
    assert_eq!(
        fs::read_to_string(fixture.candidate()).unwrap(),
        "incomplete [[draft"
    );
    fs::write(fixture.candidate(), draft_text).unwrap();
    fixture.ok(&["config", "reload"]);
    assert!(
        fixture.config().forward("first").is_none(),
        "old draft must not resurrect a deleted forward"
    );
}

#[test]
fn explicit_up_overrides_an_earlier_stop_without_overwriting_pending_edits() {
    let fixture = Fixture::new();
    fixture.add("web", "31021", true);
    let mut draft = fixture.config();
    draft.defaults.retry.max_delay_secs = 62;
    let text = toml::to_string_pretty(&draft).unwrap();
    fs::write(fixture.candidate(), &text).unwrap();
    fixture.ok(&["down", "web"]);
    fixture.ok(&["up", "web"]);
    assert_eq!(fs::read_to_string(fixture.candidate()).unwrap(), text);
    fixture.ok(&["config", "reload"]);
    let config = fixture.config();
    assert_eq!(
        config.forward("web").unwrap().desired_state,
        DesiredState::Running
    );
    assert_eq!(config.defaults.retry.max_delay_secs, 62);
}

#[test]
fn retry_reports_skipped_rules_and_restart_activates_the_selected_rule() {
    let fixture = Fixture::new();
    fixture.add("paused", "31031", false);
    assert!(!fixture.running(), "disabled additions must remain offline");
    fixture.ok(&["daemon", "start"]);
    let reply = fixture.ok(&["retry", "paused"]);
    assert!(reply["data"]["affected"].as_array().unwrap().is_empty());
    assert_eq!(reply["data"]["skipped"][0]["name"], "paused");
    assert!(
        reply["data"]["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("stopped")
    );
    fixture.ok(&["restart", "paused"]);
    assert_eq!(
        fixture.config().forward("paused").unwrap().desired_state,
        DesiredState::Running
    );
    fixture.ok(&["daemon", "stop"]);
    let retry = fixture.run(&["retry", "paused"]);
    assert!(!retry.status.success());
    assert!(
        !fixture.running(),
        "retry must not implicitly start a stopped daemon"
    );
}

#[test]
fn json_argument_errors_are_one_failure_object_with_no_success_output() {
    let fixture = Fixture::new();
    let output = fixture.run(&["edit", "missing", "--tgt"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let failure: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(failure["ok"], false);
    assert_eq!(failure["error"]["code"], "invalid_arguments");
}
