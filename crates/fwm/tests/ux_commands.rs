//! Black-box regressions for common CLI edits, batches, and structured outcomes.
//! Every instance is private and only connects to a refused loopback SSH port.
use std::{
    fs,
    net::TcpListener,
    path::PathBuf,
    process::{Command, Output},
};

use fwm_core::model::{Config, DesiredState};
use serde_json::Value;

struct Cli {
    directory: tempfile::TempDir,
    ssh_config: PathBuf,
}

impl Cli {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let ssh_config = directory.path().join("ssh_config");
        let closed_port = TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        fs::write(&ssh_config, format!(
            "Host dev new-alias\n HostName 127.0.0.1\n User fixture\n Port {closed_port}\n IdentityAgent none\n"
        )).unwrap();
        Self {
            directory,
            ssh_config,
        }
    }

    fn output(&self, arguments: &[&str], json: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fwm"));
        command.arg("--config-dir").arg(self.directory.path());
        if json {
            command.arg("--json");
        }
        command.args(arguments).output().unwrap()
    }

    fn json(&self, arguments: &[&str]) -> Value {
        let output = self.output(arguments, true);
        assert!(
            output.status.success(),
            "{arguments:?}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "expected one JSON value for {arguments:?}: {error}: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
        assert!(result.is_object());
        result
    }

    fn add(&self, flags: &[&str]) {
        let mut args = vec![
            "add",
            "--server",
            "dev",
            "--ssh-config",
            self.ssh_config.to_str().unwrap(),
            "--disabled",
        ];
        args.extend(flags);
        let result = self.json(&args);
        assert_eq!(result["data"]["state"], "stopped");
        assert_eq!(result["data"]["saved"], true);
    }

    fn config(&self) -> Config {
        serde_json::from_value(self.json(&["config", "export"])).unwrap()
    }

    fn text(&self, arguments: &[&str]) -> String {
        let output = self.output(arguments, false);
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn wait_failure(&self, arguments: &[&str]) -> Value {
        let output = self.output(arguments, true);
        assert!(!output.status.success());
        assert!(
            output.stdout.is_empty(),
            "must not print an early success: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let result: Value = serde_json::from_slice(&output.stderr).unwrap_or_else(|error| {
            panic!(
                "expected one final JSON failure: {error}: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
        assert_eq!(result["ok"], false);
        assert_eq!(result["data"]["saved"], true);
        assert_eq!(result["data"]["ready"], false);
        assert_eq!(result["error"]["code"], "wait_timeout");
        result
    }
}

impl Drop for Cli {
    fn drop(&mut self) {
        let _ = self.output(&["daemon", "stop"], false);
    }
}

#[test]
fn batch_groups_support_short_and_explicit_query_selectors() {
    let cli = Cli::new();
    cli.add(&["--name", "web", "--local", "--port", "31000-31002"]);
    let config = cli.config();
    assert_eq!(config.forwards.len(), 3);
    assert!(
        config
            .forwards
            .iter()
            .all(|rule| rule.group.as_deref() == Some("web"))
    );
    for args in [
        vec!["status", "web"],
        vec!["status", "--group", "web"],
        vec!["status", "--server", "dev"],
    ] {
        let result = cli.json(&args);
        let names: Vec<_> = result["forwards"]
            .as_array()
            .unwrap()
            .iter()
            .map(|rule| rule["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["web-31000", "web-31001", "web-31002"]);
    }
    let single = cli.json(&["status", "web-31001"]);
    assert_eq!(single["forwards"].as_array().unwrap().len(), 1);
}

#[test]
fn partial_rule_edits_preserve_addresses_identity_and_batch_membership() {
    let cli = Cli::new();
    cli.add(&["--name", "web", "--local", "--port", "31000-31001"]);
    // Give one batch member a non-default hostname and IPv6 bind so losing
    // either during a later one-field edit is observable.
    cli.json(&[
        "edit",
        "web-31000",
        "--local=[::1]:31000:db.internal.example:9000",
    ]);
    let original = cli.config().forward("web-31000").unwrap().clone();
    cli.json(&["edit", "web-31000", "--tgt", "8080"]);
    let changed = cli.config().forward("web-31000").unwrap().clone();
    assert_eq!(changed.tunnel.listen(), original.tunnel.listen());
    assert_eq!(
        changed.tunnel.target().unwrap().to_string(),
        "db.internal.example:8080"
    );
    assert_eq!(changed.id, original.id);
    assert_eq!(changed.group, original.group);
    cli.json(&["edit", "web-31000", "--src", "31005"]);
    let changed = cli.config().forward("web-31000").unwrap().clone();
    assert_eq!(changed.tunnel.listen().to_string(), "[::1]:31005");
    assert_eq!(
        changed.tunnel.target().unwrap().to_string(),
        "db.internal.example:8080"
    );
    cli.json(&["edit", "web-31000", "--remote"]);
    let changed = cli.config().forward("web-31000").unwrap().clone();
    assert!(changed.tunnel.is_remote());
    assert_eq!(changed.tunnel.listen().to_string(), "[::1]:31005");
    assert_eq!(
        changed.tunnel.target().unwrap().to_string(),
        "db.internal.example:8080"
    );
    assert_eq!(changed.group.as_deref(), Some("web"));
    assert_eq!(changed.id, original.id);
    assert_eq!(changed.desired_state, DesiredState::Stopped);
    let text = cli.text(&["status", "web-31000"]);
    assert!(
        text.contains("remote [::1]:31005 -> local db.internal.example:8080"),
        "{text}"
    );
}

#[test]
fn group_edits_update_all_members_atomically_and_preserve_their_addresses() {
    let cli = Cli::new();
    cli.add(&["--name", "web", "--local", "--port", "31000-31001"]);
    cli.json(&[
        "edit",
        "web-31000",
        "--local=[::1]:31000:db.internal.example:9000",
    ]);
    let before = cli.config();
    cli.json(&["edit", "web", "--tgt", "8080"]);
    let after = cli.config();
    assert_eq!(after.revision, before.revision + 1);
    for old in &before.forwards {
        let new = after.forward(&old.id).unwrap();
        assert_eq!(new.tunnel.listen(), old.tunnel.listen());
        assert_eq!(
            new.tunnel.target().unwrap().host,
            old.tunnel.target().unwrap().host
        );
        assert_eq!(new.tunnel.target().unwrap().port, 8080);
        assert_eq!(new.group, old.group);
    }
    cli.json(&["edit", "web", "--remote"]);
    let remote = cli.config();
    assert!(remote.forwards.iter().all(|rule| rule.tunnel.is_remote()));
    for old in &after.forwards {
        let new = remote.forward(&old.id).unwrap();
        assert_eq!(new.tunnel.listen(), old.tunnel.listen());
        assert_eq!(new.tunnel.target(), old.tunnel.target());
    }
    cli.json(&["edit", "web", "--rename", "frontend"]);
    let renamed = cli.config();
    for old in &remote.forwards {
        let new = renamed.forward(&old.id).unwrap();
        assert_eq!(new.name, old.name);
        assert_eq!(new.group.as_deref(), Some("frontend"));
        assert_eq!(new.tunnel, old.tunnel);
    }
    assert_eq!(
        cli.json(&["status", "frontend"])["forwards"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn server_edits_preserve_identity_and_forward_references() {
    let cli = Cli::new();
    cli.add(&["--name", "web", "--local", "--port", "31000"]);
    let before = cli.config();
    cli.json(&[
        "server",
        "edit",
        "dev",
        "--rename",
        "production",
        "--port",
        "2200",
        "--user",
        "alice",
    ]);
    let after = cli.config();
    assert_eq!(after.servers.len(), 1);
    let server = &after.servers[0];
    assert_eq!(server.id, before.servers[0].id);
    assert_eq!(server.name, "production");
    assert_eq!(server.port, Some(2200));
    assert_eq!(server.user.as_deref(), Some("alice"));
    assert_eq!(server.ssh_alias, before.servers[0].ssh_alias);
    assert_eq!(after.forwards, before.forwards);
    let status = cli.json(&["status", "--server", "production"]);
    assert_eq!(status["forwards"][0]["server"], "production");
}

#[test]
fn moving_to_a_direct_alias_creates_the_profile_and_rule_change_in_one_revision() {
    let cli = Cli::new();
    cli.add(&["--name", "web", "--local", "--port", "31000"]);
    let before = cli.config();
    cli.json(&[
        "edit",
        "web",
        "--server",
        "new-alias",
        "--ssh-config",
        cli.ssh_config.to_str().unwrap(),
    ]);
    let after = cli.config();
    assert_eq!(after.revision, before.revision + 1);
    assert_eq!(after.servers.len(), 2);
    let new_server = after.server("new-alias").unwrap();
    assert_eq!(new_server.ssh_alias.as_deref(), Some("new-alias"));
    assert_eq!(
        new_server.ssh_config.as_deref(),
        Some(std::fs::canonicalize(&cli.ssh_config).unwrap().as_path())
    );
    assert_eq!(after.forwards[0].id, before.forwards[0].id);
    assert_eq!(after.forwards[0].server_id, new_server.id);
    assert_eq!(after.forwards[0].tunnel, before.forwards[0].tunnel);
    assert_eq!(after.forwards[0].desired_state, DesiredState::Stopped);
}

#[test]
fn status_text_displays_complete_local_and_remote_mappings() {
    let cli = Cli::new();
    cli.add(&["--name", "local", "--local=31000:db.internal.example:5432"]);
    cli.add(&[
        "--name",
        "remote",
        "--remote=31001:app.internal.example:8080",
    ]);
    let text = cli.text(&["status"]);
    assert!(
        text.contains("local 127.0.0.1:31000 -> remote db.internal.example:5432"),
        "{text}"
    );
    assert!(
        text.contains("remote 127.0.0.1:31001 -> local app.internal.example:8080"),
        "{text}"
    );
}

#[test]
fn add_requires_an_explicit_server_even_when_only_one_is_saved() {
    let cli = Cli::new();
    cli.add(&["--name", "existing", "--local", "--port", "31000"]);
    let before = cli.config();
    let result = cli.output(&["add", "--local", "--port", "31001", "--disabled"], true);
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    let error: Value = serde_json::from_slice(&result.stderr).unwrap();
    assert_eq!(error["ok"], false);
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--server")
    );
    assert_eq!(cli.config(), before);
}

#[test]
fn add_and_up_wait_failures_emit_only_the_final_json_result() {
    let cli = Cli::new();
    cli.wait_failure(&[
        "add",
        "--name",
        "waiting-add",
        "--server",
        "dev",
        "--ssh-config",
        cli.ssh_config.to_str().unwrap(),
        "--local",
        "--port",
        "31000",
        "--wait",
        "--timeout",
        "200ms",
    ]);
    assert_eq!(
        cli.config().forward("waiting-add").unwrap().desired_state,
        DesiredState::Running
    );
    cli.add(&["--name", "waiting-up", "--local", "--port", "31001"]);
    cli.wait_failure(&["up", "waiting-up", "--wait", "--timeout", "200ms"]);
    assert_eq!(
        cli.config().forward("waiting-up").unwrap().desired_state,
        DesiredState::Running
    );
}
