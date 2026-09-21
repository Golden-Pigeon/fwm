//! CLI regressions for partial shorthand edits and persistent groups. Rules are
//! disabled and each test owns a temporary configuration and daemon instance.
use fwm_core::model::{Config, ConnectionMode, DesiredState, RemoteCleanup, Tunnel};
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
fn remote_dynamic_persists_exports_and_edits_without_losing_rule_identity() {
    let cli = Fixture::new();
    cli.add(&[
        "--name",
        "school-socks",
        "--group",
        "proxies",
        "--remote-dynamic",
        "127.0.0.1:7897",
    ]);
    let config = cli.config();
    let original = config.forward("school-socks").unwrap();
    assert!(matches!(original.tunnel, Tunnel::RemoteDynamic { .. }));
    assert_eq!(original.tunnel.listen().to_string(), "127.0.0.1:7897");
    assert_eq!(original.remote_cleanup, RemoteCleanup::Verified);
    assert_eq!(original.connection_mode, ConnectionMode::Dedicated);
    let exported = cli.ok(&["config", "export"]);
    assert_eq!(exported["forwards"][0]["kind"], "remote_dynamic");
    assert!(exported["forwards"][0].get("target").is_none());
    let durable: Config =
        toml::from_str(&fs::read_to_string(cli.0.path().join("config.toml")).unwrap()).unwrap();
    assert_eq!(durable, config);

    cli.ok(&["edit", "school-socks", "--remote-dynamic", "[::1]:7898"]);
    let changed = cli.config().forward("school-socks").unwrap().clone();
    assert_eq!(changed.id, original.id);
    assert_eq!(changed.group, original.group);
    assert_eq!(changed.desired_state, DesiredState::Stopped);
    assert_eq!(changed.tunnel.listen().to_string(), "[::1]:7898");
    cli.reject_unchanged(&["edit", "school-socks", "--remote"], "requires --tgt");
    cli.ok(&["edit", "school-socks", "--remote", "--tgt", "8080"]);
    let fixed = cli.config().forward("school-socks").unwrap().clone();
    assert!(matches!(fixed.tunnel, Tunnel::Remote { .. }));
    assert_eq!(fixed.tunnel.listen(), changed.tunnel.listen());
    assert_eq!(fixed.tunnel.target().unwrap().to_string(), "localhost:8080");

    cli.ok(&[
        "edit",
        "school-socks",
        "--remote-dynamic",
        "7897",
        "--remote-cleanup",
        "off",
        "--connection-mode",
        "shared",
    ]);
    let shared = cli.config().forward("school-socks").unwrap().clone();
    assert_eq!(shared.remote_cleanup, RemoteCleanup::Off);
    assert_eq!(shared.connection_mode, ConnectionMode::Shared);
    cli.ok(&["edit", "school-socks", "--remote=7899"]);
    let fixed = cli.config().forward("school-socks").unwrap().clone();
    assert_eq!(fixed.remote_cleanup, RemoteCleanup::Off);
    assert_eq!(fixed.tunnel.target().unwrap().to_string(), "localhost:7899");
    cli.ok(&["edit", "school-socks", "--dynamic", "1080"]);
    cli.ok(&["edit", "school-socks", "--remote-dynamic", "7897"]);
    let remote = cli.config().forward("school-socks").unwrap().clone();
    assert_eq!(remote.remote_cleanup, RemoteCleanup::Verified);
    assert_eq!(remote.connection_mode, ConnectionMode::Dedicated);
    cli.ok(&["edit", "school-socks", "--dynamic", "1080"]);
    assert_eq!(
        cli.config().forward("school-socks").unwrap().remote_cleanup,
        RemoteCleanup::Off
    );
}

#[test]
fn remote_dynamic_listener_validation_is_atomic_and_remote_shorthand_stays_fixed() {
    let cli = Fixture::new();
    cli.add(&["--name", "fixed", "--remote", "7897"]);
    let config = cli.config();
    let fixed = config.forward("fixed").unwrap();
    assert!(matches!(fixed.tunnel, Tunnel::Remote { .. }));
    assert_eq!(fixed.tunnel.target().unwrap().to_string(), "localhost:7897");
    cli.reject_unchanged(
        &["edit", "fixed", "--remote=127.0.0.1:7897"],
        "use --remote-dynamic",
    );
    for spec in [
        "0",
        "65536",
        "7897-7898",
        "7897,7898",
        "localhost:7897",
        "[::1:7897",
    ] {
        cli.reject_unchanged(&["edit", "fixed", "--remote-dynamic", spec], "listen");
    }
    cli.reject_unchanged(
        &[
            "add",
            "--server",
            "dev",
            "--name",
            "invalid",
            "--remote-dynamic",
            "0",
            "--disabled",
        ],
        "listen port",
    );
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
    cli.add(&["--local", "--port", "3005-3006", "--group", "web"]);
    cli.add(&["--dynamic", "1080", "--name", "socks", "--group", "web"]);
    let config = cli.config();
    let selected = config.select_group_forwards("web").unwrap();
    assert_eq!(selected.len(), 8);
    assert_eq!(
        cli.ok(&["status", "web"])["forwards"]
            .as_array()
            .unwrap()
            .len(),
        8
    );
    assert_eq!(
        cli.ok(&["status", "--group", "web"])["forwards"]
            .as_array()
            .unwrap()
            .len(),
        8
    );
    assert!(config.forward("api-3003").is_some());
    let mut appended_names = std::collections::HashSet::new();
    for source in [3002, 3005, 3006] {
        let appended = config
            .forwards
            .iter()
            .find(|rule| rule.tunnel.listen().port() == source)
            .unwrap();
        assert!((3..=8).contains(&appended.name.len()));
        assert!(
            appended
                .name
                .bytes()
                .all(|letter| letter.is_ascii_lowercase())
        );
        assert!(appended_names.insert(&appended.name));
        assert_eq!(appended.group.as_deref(), Some("web"));
        assert_eq!(
            cli.ok(&["status", &appended.name])["forwards"][0]["id"],
            appended.id
        );
    }
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
