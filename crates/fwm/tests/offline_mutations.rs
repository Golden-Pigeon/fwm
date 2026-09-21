//! CLI side effects are part of the contract, including rejected requests.
use fwm_core::{
    model::{
        Config, ConnectionMode, DesiredState, ForwardSpec, RemoteCleanup, ServerProfile, Tunnel,
        new_id,
    },
    paths::Paths,
    store::Store,
};
use serde_json::Value;
use std::{
    fs,
    process::{Command, Output},
};

struct Fixture {
    dir: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join("ssh_config");
        fs::write(
            &ssh,
            "Host *\n IdentityAgent none\n GlobalKnownHostsFile none\n",
        )
        .unwrap();
        let mut server = ServerProfile::new("dev");
        server.host = Some("127.0.0.1".into());
        server.port = Some(1);
        server.user = Some("fixture".into());
        server.ssh_config = Some(ssh);
        server.known_hosts = Some(dir.path().join("known_hosts"));
        let config = Config {
            forwards: vec![ForwardSpec {
                id: new_id(),
                name: "old".into(),
                group: None,
                server_id: server.id.clone(),
                tunnel: Tunnel::Dynamic {
                    listen: "127.0.0.1:31991".parse().unwrap(),
                },
                desired_state: DesiredState::Running,
                connection_mode: ConnectionMode::Shared,
                remote_cleanup: RemoteCleanup::Off,
            }],
            servers: vec![server],
            ..Config::default()
        };
        Store::new(Paths::new(Some(dir.path().into())).unwrap())
            .commit(&config)
            .unwrap();
        Self { dir }
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fwm"))
            .arg("--config-dir")
            .arg(self.dir.path())
            .arg("--json")
            .args(args)
            .output()
            .unwrap()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn config(&self) -> Config {
        serde_json::from_value(self.ok(&["config", "export"])).unwrap()
    }
    fn stopped(&self) {
        assert_eq!(self.ok(&["daemon", "status"])["daemon_running"], false);
        assert!(
            !self.dir.path().join("state/daemon.log").exists(),
            "daemon must never have been launched"
        );
    }
    fn unchanged(&self, before: &Config) {
        assert_eq!(self.config(), *before);
        self.stopped();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.run(&["daemon", "stop"]);
    }
}

#[test]
fn configuration_only_commands_leave_saved_running_rules_dormant() {
    let f = Fixture::new();
    f.ok(&[
        "add",
        "new",
        "--server",
        "dev",
        "--local",
        "--port",
        "31992",
        "--disabled",
    ]);
    f.stopped();
    f.ok(&["edit", "new", "--tgt", "8080"]);
    f.stopped();
    f.ok(&["edit", "old", "--rename", "renamed"]);
    f.stopped();
    f.ok(&["server", "edit", "dev", "--port", "2"]);
    f.stopped();
    f.ok(&[
        "server",
        "add",
        "unused",
        "--host",
        "127.0.0.1",
        "--port",
        "1",
    ]);
    f.stopped();
    f.ok(&["server", "remove", "unused"]);
    f.stopped();
    assert_eq!(
        f.config().forward("renamed").unwrap().desired_state,
        DesiredState::Running
    );
    assert_eq!(
        f.config().forward("new").unwrap().desired_state,
        DesiredState::Stopped
    );
    let events = f.ok(&["logs", "new"]);
    assert!(!events["events"].as_array().unwrap().is_empty());
}

#[test]
fn invalid_commands_do_not_launch_daemon_or_change_configuration() {
    let f = Fixture::new();
    let before = f.config();
    for args in [
        vec!["up", "typo"],
        vec!["restart", "typo"],
        vec!["up", "--server", "typo"],
        vec!["restart", "--group", "typo"],
        vec!["edit", "typo", "--tgt", "8080"],
        vec!["edit", "old", "--rename", "invalid name"],
        vec!["server", "add", "dev", "--host", "127.0.0.1"],
        vec!["server", "edit", "typo", "--port", "22"],
        vec!["server", "remove", "dev"],
        vec!["server", "remove", "typo"],
        vec![
            "add", "old", "--server", "dev", "--local", "--port", "31993",
        ],
        vec![
            "add",
            "--server",
            "invalid alias",
            "--local",
            "--port",
            "31993",
        ],
        vec!["add", "--server", "dev", "--local", "--port", "0"],
    ] {
        let result = f.run(&args);
        assert!(!result.status.success(), "{args:?} should fail");
        assert!(
            result.stdout.is_empty(),
            "failure must have only its final JSON error"
        );
        let error: Value = serde_json::from_slice(&result.stderr).unwrap();
        assert_eq!(error["ok"], false);
        f.unchanged(&before);
    }
}

#[test]
fn empty_or_identical_edits_are_noops_offline_and_online() {
    let f = Fixture::new();
    let before = f.config();
    for args in [
        vec!["edit", "old"],
        vec!["server", "edit", "dev"],
        vec!["server", "edit", "dev", "--port", "1"],
    ] {
        let result = f.ok(&args);
        assert!(
            result["data"]["message"]
                .as_str()
                .unwrap()
                .contains("No changes")
        );
        f.unchanged(&before);
    }
    f.ok(&["down", "old"]);
    f.ok(&["daemon", "start"]);
    let before = f.config();
    f.ok(&["edit", "old"]);
    f.ok(&["server", "edit", "dev"]);
    assert_eq!(
        f.config(),
        before,
        "no-op must not bump revision or reconcile"
    );
}

#[test]
fn offline_edits_reject_pending_drafts_without_overwriting_them() {
    let f = Fixture::new();
    let draft = "unfinished = [\n";
    fs::write(f.dir.path().join("config.toml"), draft).unwrap();
    for args in [
        vec!["edit", "old", "--rename", "renamed"],
        vec!["server", "edit", "dev", "--port", "2"],
        vec![
            "add",
            "--server",
            "dev",
            "--local",
            "--port",
            "31992",
            "--disabled",
        ],
    ] {
        let result = f.run(&args);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("config_pending_edits"));
        let failure: Value = serde_json::from_slice(&result.stderr).unwrap();
        assert_eq!(failure["ok"], false, "draft warnings must not corrupt JSON");
        assert_eq!(
            fs::read_to_string(f.dir.path().join("config.toml")).unwrap(),
            draft
        );
        f.stopped();
    }
}

#[test]
fn offline_status_reports_fallback_in_json_without_mixing_text() {
    let f = Fixture::new();
    fs::write(f.dir.path().join("config.toml"), "unfinished = [").unwrap();
    let output = f.run(&["status"]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["daemon_running"], false);
    assert_eq!(status["forwards"][0]["name"], "old");
    assert!(
        status["warnings"][0]
            .as_str()
            .unwrap()
            .contains("candidate is invalid")
    );
    f.stopped();
}

#[test]
fn checks_and_failed_trust_do_not_start_saved_rules() {
    let f = Fixture::new();
    for args in [
        vec!["server", "check", "dev"],
        vec!["doctor"],
        vec!["server", "trust", "dev", "--fingerprint", "SHA256:invalid"],
    ] {
        assert!(!f.run(&args).status.success());
        f.stopped();
    }
}

#[test]
fn valid_up_and_restart_start_after_persisting_intent() {
    for command in ["up", "restart"] {
        let f = Fixture::new();
        f.ok(&["down", "old"]);
        f.stopped();
        f.ok(&[command, "old"]);
        assert_eq!(f.ok(&["daemon", "status"])["daemon_running"], true);
        assert_eq!(
            f.config().forward("old").unwrap().desired_state,
            DesiredState::Running
        );
    }
}

#[test]
fn offline_reload_applies_stopped_and_deleted_rules_without_starting_old_intent() {
    let f = Fixture::new();
    let paths = Paths::new(Some(f.dir.path().into())).unwrap();
    let store = Store::new(paths.clone());
    let mut applied = f.config();
    let mut deleted = applied.forwards[0].clone();
    deleted.id = new_id();
    deleted.name = "removed".into();
    deleted.tunnel = Tunnel::Dynamic {
        listen: "127.0.0.1:31992".parse().unwrap(),
    };
    applied.forwards.push(deleted);
    applied.revision = 7;
    store.commit(&applied).unwrap();
    let mut candidate = applied.clone();
    candidate.forwards.retain(|rule| rule.name != "removed");
    candidate.forwards[0].desired_state = DesiredState::Stopped;
    candidate.defaults.retry.max_delay_secs = 63;
    let draft = format!(
        "# intentional pending edits\n{}",
        toml::to_string_pretty(&candidate).unwrap()
    );
    fs::write(&paths.config_file, &draft).unwrap();
    let rejected = f.run(&["server", "edit", "dev", "--port", "2"]);
    assert!(!rejected.status.success());
    assert_eq!(fs::read_to_string(&paths.config_file).unwrap(), draft);
    assert_eq!(f.config(), applied);
    f.stopped();
    let result = f.ok(&["config", "reload"]);
    let after = f.config();
    assert_eq!(after.revision, applied.revision + 1);
    assert_eq!(result["data"]["revision"], after.revision);
    assert_eq!(after.forwards.len(), 1);
    assert_eq!(
        after.forward("old").unwrap().desired_state,
        DesiredState::Stopped
    );
    assert!(after.forward("removed").is_none());
    assert_eq!(after.defaults.retry.max_delay_secs, 63);
    assert_eq!(store.read_candidate().unwrap(), after);
    f.stopped();
}

#[test]
fn invalid_offline_reload_preserves_draft_bytes_and_committed_running_rules() {
    let f = Fixture::new();
    let before = f.config();
    let snapshot = f.dir.path().join("state/applied.toml");
    let snapshot_bytes = fs::read(&snapshot).unwrap();
    for draft in ["# work in progress\nforwards = [", "schema_version = 999\n"] {
        fs::write(f.dir.path().join("config.toml"), draft).unwrap();
        let result = f.run(&["config", "reload"]);
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        let failure: Value = serde_json::from_slice(&result.stderr).unwrap();
        assert_eq!(failure["ok"], false);
        assert_eq!(
            fs::read_to_string(f.dir.path().join("config.toml")).unwrap(),
            draft
        );
        assert_eq!(fs::read(&snapshot).unwrap(), snapshot_bytes);
        f.unchanged(&before);
    }
}
