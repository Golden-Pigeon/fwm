//! CLI regressions for partial shorthand edits and persistent groups. Rules are
//! disabled and each test owns a temporary configuration and daemon instance.
use fwm_core::model::{Config, DesiredState};
use serde_json::Value;
use std::{
    fs,
    process::{Command, Output},
};

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new() -> Self {
        let fixture = Self(tempfile::tempdir().unwrap());
        fs::write(
            fixture.0.path().join("ssh_config"),
            "Host dev other\n HostName 127.0.0.1\n User fixture\n Port 1\n IdentityAgent none\n",
        )
        .unwrap();
        fixture
    }

    fn output(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fwm"))
            .arg("--config-dir")
            .arg(self.0.path())
            .arg("--json")
            .args(args)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> Value {
        let output = self.output(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn add(&self, args: &[&str]) {
        let path = self.0.path().join("ssh_config");
        let mut arguments = vec![
            "add",
            "--server",
            "dev",
            "--ssh-config",
            path.to_str().unwrap(),
            "--disabled",
        ];
        arguments.extend(args);
        self.ok(&arguments);
    }

    fn config(&self) -> Config {
        serde_json::from_value(self.ok(&["config", "export"])).unwrap()
    }

    fn reject_unchanged(&self, args: &[&str], expected: &str) {
        let before = self.config();
        let bytes = fs::read(self.0.path().join("config.toml")).unwrap();
        let output = self.output(args);
        assert!(!output.status.success(), "{args:?} unexpectedly succeeded");
        assert!(
            output.stdout.is_empty(),
            "{args:?} printed success before failing"
        );
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["ok"], false);
        assert!(error.to_string().contains(expected), "{args:?}: {error}");
        assert_eq!(self.config(), before);
        assert_eq!(fs::read(self.0.path().join("config.toml")).unwrap(), bytes);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.output(&["daemon", "stop"]);
    }
}

#[test]
fn direction_value_edit_preserves_hosts_identity_and_group_on_both_directions() {
    let cli = Fixture::new();
    cli.add(&[
        "--name",
        "web",
        "--group",
        "apps",
        "--local=[::1]:3000:db.internal:8080",
    ]);
    let original = cli.config().forward("web").unwrap().clone();
    for (flag, port, remote) in [
        ("--local=3001", 3001, false),
        ("--remote=65535", 65535, true),
        ("--local=1", 1, false),
    ] {
        cli.ok(&["edit", "web", flag]);
        let rule = cli.config().forward("web").unwrap().clone();
        assert_eq!(rule.id, original.id);
        assert_eq!(rule.group, original.group);
        assert_eq!(rule.desired_state, DesiredState::Stopped);
        assert_eq!(rule.tunnel.listen().to_string(), format!("[::1]:{port}"));
        assert_eq!(
            rule.tunnel.target().unwrap().to_string(),
            format!("db.internal:{port}")
        );
        assert_eq!(rule.tunnel.is_remote(), remote);
    }
    for invalid in [
        "--local=0",
        "--remote=65536",
        "--local=3000-3001",
        "--remote=3000,3001",
    ] {
        cli.reject_unchanged(&["edit", "web", invalid], "port");
    }
}

#[test]
fn explicit_group_supports_single_batch_and_named_appends_with_consistent_selection() {
    let cli = Fixture::new();
    cli.add(&["--local", "--port", "3000-3001", "--name", "web"]);
    cli.add(&["--local", "--port", "3002", "--group", "web"]);
    cli.add(&[
        "--local",
        "--port",
        "3003-3004",
        "--name",
        "api",
        "--group",
        "web",
    ]);
    cli.add(&["--dynamic", "1080", "--name", "socks", "--group", "web"]);
    let config = cli.config();
    let selected = config.select_group_forwards("web").unwrap();
    assert_eq!(selected.len(), 6);
    assert_eq!(
        cli.ok(&["status", "web"])["forwards"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
    assert_eq!(
        cli.ok(&["status", "--group", "web"])["forwards"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
    assert!(config.forward("api-3003").is_some());
    assert!(config.forward("dev-local-3002").is_some());
    cli.ok(&["remove", "--group", "web"]);
    assert!(cli.config().forwards.is_empty());
}

#[test]
fn stopped_group_cannot_be_collapsed_onto_one_listener_by_any_edit_form() {
    let cli = Fixture::new();
    cli.add(&["--local", "--port", "3000-3001", "--name", "web"]);
    for flags in [
        vec!["--src", "4000"],
        vec!["--port", "4000"],
        vec!["--local=4000"],
        vec!["--remote=4000"],
        vec!["--local=127.0.0.1:4000:db.internal:8080"],
        vec!["--dynamic", "4000"],
    ] {
        let mut args = vec!["edit", "web"];
        args.extend(flags);
        cli.reject_unchanged(&args, "conflict within group");
    }
    cli.ok(&["edit", "web", "--tgt", "8080"]);
    let config = cli.config();
    assert_eq!(
        config
            .forwards
            .iter()
            .map(|rule| rule.tunnel.listen().port())
            .collect::<Vec<_>>(),
        [3000, 3001]
    );
    assert!(
        config
            .forwards
            .iter()
            .all(|rule| rule.tunnel.target().unwrap().port == 8080)
    );
}

#[test]
fn group_errors_do_not_change_config_and_explain_the_correct_append_syntax() {
    let cli = Fixture::new();
    cli.add(&["--local", "--port", "3000-3001", "--name", "web"]);
    cli.reject_unchanged(
        &[
            "add",
            "--server",
            "dev",
            "--disabled",
            "--local",
            "--port",
            "3002",
            "--name",
            "web",
        ],
        "--group web",
    );
    cli.reject_unchanged(
        &[
            "add",
            "--server",
            "dev",
            "--disabled",
            "--local",
            "--port",
            "3000",
            "--name",
            "duplicate",
            "--group",
            "web",
        ],
        "conflict within group",
    );
    cli.reject_unchanged(
        &[
            "add",
            "--server",
            "dev",
            "--disabled",
            "--local",
            "--port",
            "3002",
            "--group",
            "web-3000",
        ],
        "conflicts with an individual forward",
    );
}

#[test]
fn different_stopped_groups_can_share_ports_and_member_moves_cannot_make_a_group_invalid() {
    let cli = Fixture::new();
    cli.add(&[
        "--local", "--port", "3000", "--name", "first", "--group", "web",
    ]);
    cli.add(&[
        "--local",
        "--port",
        "3000",
        "--name",
        "alternative",
        "--group",
        "alternatives",
    ]);
    cli.add(&[
        "--remote", "--port", "3000", "--name", "second", "--group", "web",
    ]);
    assert_eq!(cli.config().forwards.len(), 3);
    cli.reject_unchanged(&["edit", "second", "--local"], "conflict within group");
    cli.ok(&["edit", "second", "--remote=3001"]);
}
