//! Regression contracts for the numbered UX audit. All state is private and
//! SSH destinations are an intentionally closed loopback port.
use fwm_core::model::Config;
use serde_json::Value;
use std::{
    fs,
    process::{Command, Output},
};

struct Fixture(tempfile::TempDir);
impl Fixture {
    fn new() -> Self {
        let fixture = Self(tempfile::tempdir().unwrap());
        // Closed-loopback failures must not depend on the developer's SSH
        // configuration or agent settings.
        let ssh_config = fixture.0.path().join("ssh_config");
        fs::write(
            &ssh_config,
            "Host *\n IdentityAgent none\n IdentityFile none\n",
        )
        .unwrap();
        let closed = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = closed.local_addr().unwrap().port().to_string();
        drop(closed);
        for name in ["dev", "other"] {
            fixture.ok(&[
                "server",
                "add",
                name,
                "--host",
                "127.0.0.1",
                "--port",
                &port,
                "--user",
                "fixture",
                "--ssh-config",
                ssh_config.to_str().unwrap(),
            ]);
        }
        fixture
    }
    fn run(&self, args: &[&str], json: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fwm"));
        command.arg("--config-dir").arg(self.0.path());
        if json {
            command.arg("--json");
        }
        command.args(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let output = self.run(args, true);
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
    fn add(&self, server: &str, name: &str, group: &str, direction: &str, port: &str) {
        self.ok(&[
            "add",
            "--server",
            server,
            "--name",
            name,
            "--group",
            group,
            direction,
            "--port",
            port,
            "--disabled",
        ]);
    }
    fn unchanged_failure(&self, args: &[&str], needle: &str) {
        let before = self.config();
        let bytes = fs::read(self.0.path().join("config.toml")).unwrap();
        let output = self.run(args, true);
        assert!(!output.status.success(), "{args:?}");
        assert!(output.stdout.is_empty());
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert!(error.to_string().contains(needle), "{args:?}: {error}");
        assert_eq!(self.config(), before);
        assert_eq!(fs::read(self.0.path().join("config.toml")).unwrap(), bytes);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.run(&["daemon", "stop"], true);
    }
}

#[test]
fn u01_u08_group_rename_preserves_custom_names_and_discovery_reports_membership() {
    let cli = Fixture::new();
    cli.add("dev", "api", "apps", "--remote", "3000");
    cli.add("other", "database", "apps", "--remote", "3000");
    let before = cli.config();
    cli.ok(&["daemon", "start"]);
    cli.ok(&["edit", "apps", "--rename", "services"]);
    let after = cli.config();
    for rule in &before.forwards {
        let changed = after.forward(&rule.id).unwrap();
        assert_eq!(changed.name, rule.name);
        assert_eq!(changed.tunnel, rule.tunnel);
        assert_eq!(changed.desired_state, rule.desired_state);
        assert_eq!(changed.group.as_deref(), Some("services"));
    }
    let groups = cli.ok(&["group", "list"]);
    assert_eq!(groups["groups"].as_array().unwrap().len(), 1);
    assert_eq!(groups["groups"][0]["name"], "services");
    assert_eq!(groups["groups"][0]["members"].as_array().unwrap().len(), 2);
    assert_eq!(
        groups["groups"][0]["servers"],
        serde_json::json!(["dev", "other"])
    );
    let listing = cli.run(&["group", "list"], false);
    let listing = String::from_utf8_lossy(&listing.stdout);
    for label in ["services", "api", "database", "dev", "other"] {
        assert!(listing.contains(label));
    }
    assert!(
        cli.ok(&["status"])["forwards"]
            .as_array()
            .unwrap()
            .iter()
            .all(|rule| rule["group"] == "services")
    );
    let human = cli.run(&["status"], false);
    assert!(String::from_utf8_lossy(&human.stdout).contains("GROUP"));
    assert!(String::from_utf8_lossy(&human.stdout).contains("services"));
    cli.ok(&["daemon", "stop"]);
    assert_eq!(cli.ok(&["daemon", "status"])["daemon_running"], false);
}

#[test]
fn u02_u08_group_merge_is_explicit_and_members_can_join_move_or_leave_without_identity_loss() {
    let cli = Fixture::new();
    cli.add("dev", "api", "alpha", "--local", "3000");
    cli.add("dev", "database", "beta", "--local", "3001");
    cli.unchanged_failure(&["edit", "alpha", "--rename", "beta"], "already exists");
    cli.unchanged_failure(
        &["edit", "alpha", "--rename", "gamma", "--group", "beta"],
        "cannot be combined",
    );
    let before = cli.config();
    cli.ok(&["edit", "alpha", "--group", "beta"]);
    assert_eq!(
        cli.ok(&["status", "--group", "beta"])["forwards"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    cli.ok(&["edit", "api", "--ungroup"]);
    assert!(cli.config().forward("api").unwrap().group.is_none());
    cli.ok(&["edit", "api", "--group", "gamma"]);
    cli.ok(&["edit", "gamma", "--ungroup"]);
    for rule in &before.forwards {
        let changed = cli.config().forward(&rule.id).unwrap().clone();
        assert_eq!(changed.id, rule.id);
        assert_eq!(changed.name, rule.name);
        assert_eq!(changed.desired_state, rule.desired_state);
    }
    cli.unchanged_failure(
        &["edit", "api", "--group", "database"],
        "conflicts with an individual forward",
    );
    cli.add("dev", "alternative", "conflicting", "--local", "3000");
    cli.unchanged_failure(
        &["edit", "api", "--group", "conflicting"],
        "conflict within group",
    );
}

#[test]
fn empty_group_discovery_does_not_initialize_a_configuration_or_start_a_daemon() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("unused");
    let output = Command::new(env!("CARGO_BIN_EXE_fwm"))
        .arg("--config-dir")
        .arg(&path)
        .args(["--json", "group", "list"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["groups"],
        serde_json::json!([])
    );
    assert!(!path.exists());
}

#[test]
fn u11_u12_numeric_edit_names_and_optional_bind_patches_work_through_the_process() {
    let cli = Fixture::new();
    cli.ok(&[
        "add",
        "--server",
        "dev",
        "--name",
        "1234",
        "--local=[::1]:3000:db.internal:8080",
        "--disabled",
    ]);
    let original = cli.config().forward("1234").unwrap().clone();
    cli.ok(&["edit", "--remote", "1234"]);
    cli.ok(&["edit", "1234", "--local=3001:new.internal:8081"]);
    let changed = cli.config().forward("1234").unwrap().clone();
    assert_eq!(changed.id, original.id);
    assert_eq!(changed.tunnel.listen().to_string(), "[::1]:3001");
    assert_eq!(
        changed.tunnel.target().unwrap().to_string(),
        "new.internal:8081"
    );
}

#[test]
fn u13_u14_invalid_ipv6_and_ambiguous_object_names_fail_atomically() {
    let cli = Fixture::new();
    cli.add("dev", "first", "apps", "--local", "3000");
    cli.add("dev", "second", "apps", "--local", "3001");
    for literal in ["[2001:::1]", "[not:ipv6]"] {
        cli.unchanged_failure(
            &["edit", "first", &format!("--local=3000:{literal}:80")],
            "invalid IPv6",
        );
    }
    let config = cli.config();
    let second_id = &config.forward("second").unwrap().id;
    cli.unchanged_failure(
        &["edit", "first", "--rename", second_id],
        "another forward's ID",
    );
    cli.unchanged_failure(&["edit", "first", "--group", second_id], "forward ID");
    let other_id = &config.server("other").unwrap().id;
    cli.unchanged_failure(
        &["server", "edit", "dev", "--rename", other_id],
        "another server's ID",
    );
    assert_eq!(
        cli.ok(&["status", second_id])["forwards"][0]["name"],
        "second"
    );
}

#[test]
fn u15_automatic_long_groups_are_distinct_stable_and_never_merge_unrelated_profiles() {
    let cli = Fixture::new();
    let first = format!("{}AAAAAAA", "s".repeat(93));
    let second = format!("{}BBBBBBB", "s".repeat(93));
    for (server, ports) in [(&first, "3000-3001"), (&second, "3002-3003")] {
        cli.ok(&[
            "server",
            "add",
            server,
            "--host",
            "127.0.0.1",
            "--port",
            "1",
            "--user",
            "fixture",
        ]);
        cli.ok(&[
            "add",
            "--server",
            server,
            "--remote",
            "--port",
            ports,
            "--disabled",
        ]);
    }
    let before = cli.config();
    let group = before.forwards[0].group.clone().unwrap();
    assert_ne!(before.forwards[0].group, before.forwards[2].group);
    assert!(group.len() <= 100);
    cli.ok(&[
        "add",
        "--server",
        &first,
        "--remote",
        "--port",
        "3004-3005",
        "--disabled",
    ]);
    assert_eq!(
        cli.ok(&["status", "--group", &group])["forwards"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    cli.add("other", "foreign", "dev-local", "--remote", "9000");
    cli.unchanged_failure(
        &[
            "add",
            "--server",
            "dev",
            "--local",
            "--port",
            "4000-4001",
            "--disabled",
        ],
        "automatic group",
    );
}

#[test]
fn u16_dual_stack_groups_allow_disjoint_families_and_reject_mapped_duplicates() {
    let cli = Fixture::new();
    cli.ok(&[
        "add",
        "--server",
        "dev",
        "--name",
        "v4",
        "--group",
        "dual",
        "--local=0.0.0.0:3000:localhost:80",
        "--disabled",
    ]);
    cli.ok(&[
        "add",
        "--server",
        "dev",
        "--name",
        "v6",
        "--group",
        "dual",
        "--local=[::1]:3000:localhost:80",
        "--disabled",
    ]);
    cli.unchanged_failure(
        &[
            "add",
            "--server",
            "dev",
            "--name",
            "mapped",
            "--group",
            "dual",
            "--local=[::ffff:127.0.0.1]:3000:localhost:80",
            "--disabled",
        ],
        "conflict within group",
    );
}

#[test]
fn u10_explicit_timeout_waits_for_add_up_and_restart_and_conflicts_with_disabled() {
    let cli = Fixture::new();
    cli.unchanged_failure(
        &[
            "add",
            "--server",
            "dev",
            "--local",
            "--port",
            "3000",
            "--disabled",
            "--timeout",
            "1ms",
        ],
        "implies --wait",
    );
    for args in [
        vec![
            "add",
            "--server",
            "dev",
            "--name",
            "web",
            "--local",
            "--port",
            "3000",
            "--timeout",
            "1ms",
        ],
        vec!["up", "web", "--timeout", "1ms"],
        vec!["restart", "web", "--timeout", "1ms"],
    ] {
        let output = cli.run(&args, true);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["error"]["code"], "wait_timeout", "{args:?}: {error}");
        assert_eq!(error["data"]["saved"], true);
        assert_eq!(error["data"]["ready"], false);
    }
}

#[test]
fn u30_u31_validate_rejects_typos_and_imported_remote_defaults_match_cli() {
    let cli = Fixture::new();
    cli.add("dev", "remote", "apps", "--remote", "3000");
    let path = cli.0.path().join("config.toml");
    let saved = fs::read_to_string(&path).unwrap();
    for typo in [
        saved.replace("desired_state =", "desired_sate ="),
        saved.replace("user =", "usr ="),
    ] {
        fs::write(&path, typo).unwrap();
        let output = cli.run(&["config", "validate"], true);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("unknown field"));
    }
    let imported = saved
        .lines()
        .filter(|line| {
            !line.starts_with("remote_cleanup =") && !line.starts_with("connection_mode =")
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, &imported).unwrap();
    cli.ok(&["config", "validate"]);
    cli.ok(&["config", "reload"]);
    let rule = cli.config().forward("remote").unwrap().clone();
    assert_eq!(
        rule.remote_cleanup,
        fwm_core::model::RemoteCleanup::Verified
    );
    assert_eq!(
        rule.connection_mode,
        fwm_core::model::ConnectionMode::Dedicated
    );
    assert_eq!(cli.ok(&["daemon", "status"])["daemon_running"], false);
    fs::write(&path, format!("{imported}\nremote_cleanup = \"off\"\n")).unwrap();
    cli.ok(&["config", "reload"]);
    assert_eq!(
        cli.config().forward("remote").unwrap().remote_cleanup,
        fwm_core::model::RemoteCleanup::Off
    );
}
