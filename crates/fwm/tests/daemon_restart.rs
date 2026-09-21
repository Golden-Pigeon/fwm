//! Exercise daemon lifecycle through the actual cross-platform CLI and private IPC.
//! All daemons use separate temporary profiles; no system service is registered.
use std::{
    fs,
    net::TcpListener,
    process::{Command, Output},
};

use fwm_core::model::{Config, DesiredState};
use serde_json::Value;

struct Cli {
    directory: tempfile::TempDir,
}

impl Cli {
    fn new() -> Self {
        Self {
            directory: tempfile::tempdir().unwrap(),
        }
    }

    fn output(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fwm"))
            .arg("--config-dir")
            .arg(self.directory.path())
            .arg("--json")
            .args(arguments)
            .output()
            .unwrap()
    }

    fn success(&self, arguments: &[&str]) -> Value {
        let output = self.output(arguments);
        assert!(
            output.status.success(),
            "fwm {arguments:?} failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        // from_slice rejects trailing JSON values and non-JSON status lines.
        let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "fwm {arguments:?} must print exactly one JSON object: {error}\n{}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
        assert!(value.is_object(), "expected an object, got {value}");
        value
    }

    fn restart(&self) {
        let response = self.success(&["daemon", "restart"]);
        let message = response
            .get("message")
            .or_else(|| response.get("data").and_then(|data| data.get("message")))
            .and_then(Value::as_str);
        assert!(
            message.is_some_and(|message| message.starts_with("Daemon restarted")),
            "{response}",
        );
    }

    fn instance(&self) -> String {
        let response = self.success(&["status"]);
        assert_eq!(response["daemon_running"], true, "{response}");
        let id = response["daemon_instance_id"].as_str().unwrap();
        assert!(!id.is_empty());
        id.to_owned()
    }

    fn config(&self) -> Config {
        serde_json::from_value(self.success(&["config", "export"])).unwrap()
    }

    fn persisted(&self) -> Vec<u8> {
        fs::read(self.directory.path().join("config.toml")).unwrap()
    }

    fn add_fixture_configuration(&self) {
        let ssh_config = self.directory.path().join("fixture_ssh_config");
        fs::write(&ssh_config, "Host *\n IdentityAgent none\n").unwrap();
        let ssh_port = free_port().to_string();
        self.success(&[
            "server",
            "add",
            "fixture",
            "--host",
            "127.0.0.1",
            "--port",
            &ssh_port,
            "--user",
            "fixture",
            "--ssh-config",
            ssh_config.to_str().unwrap(),
        ]);
        let running_port = free_port().to_string();
        self.success(&[
            "add",
            "--name",
            "running",
            "--server",
            "fixture",
            "--local",
            "--port",
            &running_port,
        ]);
        let stopped_port = free_port().to_string();
        self.success(&[
            "add",
            "--name",
            "stopped",
            "--server",
            "fixture",
            "--local",
            "--port",
            &stopped_port,
            "--disabled",
        ]);
    }
}

impl Drop for Cli {
    fn drop(&mut self) {
        // Stop the daemon before its private directory is removed, including
        // after an assertion fails. This never targets another test's daemon.
        let _ = self.output(&["daemon", "stop"]);
    }
}

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn restart_starts_an_empty_profile_and_replaces_a_running_instance() {
    let cli = Cli::new();
    assert!(!cli.directory.path().join("config.toml").exists());
    cli.restart();
    let first = cli.instance();
    let saved = cli.config();
    assert!(saved.servers.is_empty() && saved.forwards.is_empty());
    cli.restart();
    assert_ne!(cli.instance(), first);
    assert_eq!(cli.config(), saved);
}

#[cfg(unix)]
#[test]
fn long_profile_path_supports_background_start_status_and_restart() {
    let cli = Cli {
        directory: tempfile::Builder::new()
            .prefix(&"long-profile-".repeat(12))
            .tempdir()
            .unwrap(),
    };
    cli.success(&["daemon", "start"]);
    let unused_endpoint = cli.directory.path().join("state/daemon.sock");
    fs::write(&unused_endpoint, "unused long socket location").unwrap();
    let first = cli.instance();
    cli.restart();
    assert_ne!(cli.instance(), first);
    cli.success(&["daemon", "stop"]);
    assert_eq!(
        cli.success(&["daemon", "status"])["daemon_state"],
        "stopped"
    );
    assert_eq!(
        fs::read_to_string(unused_endpoint).unwrap(),
        "unused long socket location"
    );
}

#[test]
fn restart_preserves_profiles_ids_revisions_and_running_or_stopped_intent() {
    let cli = Cli::new();
    cli.add_fixture_configuration();
    let before = cli.config();
    let persisted = cli.persisted();
    let first_instance = cli.instance();
    assert_eq!(before.servers.len(), 1);
    assert_eq!(before.forwards.len(), 2);
    assert_eq!(
        before.forward("running").unwrap().desired_state,
        DesiredState::Running,
    );
    assert_eq!(
        before.forward("stopped").unwrap().desired_state,
        DesiredState::Stopped,
    );
    cli.restart();
    assert_ne!(cli.instance(), first_instance);
    // Whole-value equality includes each stable server/forward ID and revision.
    assert_eq!(cli.config(), before);
    assert_eq!(cli.persisted(), persisted, "restart rewrote configuration");
    let runtime = cli.success(&["status"]);
    for (name, desired) in [("running", "running"), ("stopped", "stopped")] {
        let forward = runtime["forwards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|forward| forward["name"] == name)
            .unwrap();
        assert_eq!(forward["desired_state"], desired);
        assert_eq!(forward["id"], before.forward(name).unwrap().id);
    }
}

#[test]
fn restart_starts_a_stopped_daemon_with_its_existing_configuration() {
    let cli = Cli::new();
    cli.add_fixture_configuration();
    let before = cli.config();
    let first_instance = cli.instance();
    cli.success(&["daemon", "stop"]);
    assert_eq!(cli.success(&["status"])["daemon_running"], false);
    cli.restart();
    assert_ne!(cli.instance(), first_instance);
    assert_eq!(cli.config(), before);
}

#[test]
fn restarting_one_profile_does_not_replace_another_daemon() {
    let first = Cli::new();
    let second = Cli::new();
    first.success(&["daemon", "start"]);
    second.add_fixture_configuration();
    let first_id = first.instance();
    let second_id = second.instance();
    let second_config = second.config();
    let second_persisted = second.persisted();
    first.restart();
    assert_ne!(first.instance(), first_id);
    assert_eq!(second.instance(), second_id);
    assert_eq!(second.config(), second_config);
    assert_eq!(second.persisted(), second_persisted);
}
