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

/// A daemon one protocol generation behind refuses the newer protocol, but its
/// Shutdown encoding is unchanged; replacement must retire it over v1 instead
/// of failing the whole upgrade.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_replaces_a_daemon_one_protocol_generation_behind() {
    use fwm_api::codec::{read_frame, write_frame};
    use fwm_api::protocol::{ApiError, Command, Request, Response};
    use serde_json::json;

    let cli = Cli::new();
    let state = cli.directory.path().join("state");
    fs::create_dir_all(&state).unwrap();
    let socket = state.join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let socket_path = socket.clone();

    let legacy = tokio::spawn(async move {
        // Presence probes connect without sending; any v2 request — Ping or
        // Shutdown — gets the coherent refusal a v0.1 daemon really sends.
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let Some(request): Option<Request> = read_frame(&mut stream).await.unwrap() else {
                continue;
            };
            if request.api_version == 2 {
                write_frame(
                    &mut stream,
                    &Response {
                        api_version: 1,
                        request_id: request.request_id.clone(),
                        ok: false,
                        data: serde_json::Value::Null,
                        error: Some(ApiError {
                            code: "version_mismatch".into(),
                            message: "supported API version is 1".into(),
                        }),
                    },
                )
                .await
                .unwrap();
                continue;
            }
            assert_eq!(request.api_version, 1, "only Shutdown is downgraded");
            assert!(matches!(request.command, Command::Shutdown));
            write_frame(
                &mut stream,
                &Response {
                    api_version: 1,
                    request_id: request.request_id.clone(),
                    ok: true,
                    data: json!({}),
                    error: None,
                },
            )
            .await
            .unwrap();
            break;
        }
        drop(listener);
        // Like a real daemon, retire together with the socket.
        let _ = fs::remove_file(&socket_path);
    });

    cli.restart();
    legacy.await.unwrap();
    let response = cli.success(&["status"]);
    assert_eq!(response["daemon_running"], true, "{response}");
}

/// A coherent daemon from a newer protocol is named as such and is never
/// replaced; no second daemon may be started over its socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_refuses_to_replace_a_newer_protocol_daemon() {
    use fwm_api::codec::{read_frame, write_frame};
    use fwm_api::protocol::{ApiError, Request, Response};

    let cli = Cli::new();
    let state = cli.directory.path().join("state");
    fs::create_dir_all(&state).unwrap();
    let socket = state.join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();

    let newer = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            let Some(request): Option<Request> = read_frame(&mut stream).await.unwrap() else {
                continue;
            };
            write_frame(
                &mut stream,
                &Response {
                    api_version: 3,
                    request_id: request.request_id.clone(),
                    ok: false,
                    data: serde_json::Value::Null,
                    error: Some(ApiError {
                        code: "version_mismatch".into(),
                        message: "supported API version is 3".into(),
                    }),
                },
            )
            .await
            .unwrap();
        }
    });

    let output = cli.output(&["daemon", "restart"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("daemon_api_unsupported"), "{stderr}");
    assert!(stderr.contains("upgrade fwm on this machine"), "{stderr}");
    // The unreplaceable owner keeps its socket; no second daemon started.
    assert!(socket.exists());
    newer.abort();
}
