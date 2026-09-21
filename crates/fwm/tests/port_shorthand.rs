//! Exercise shorthand through the public CLI, daemon IPC, and durable config.
//! All rules are disabled so these tests need neither an SSH server nor free ports.

use fwm_core::model::{Config, DesiredState, ForwardSpec, Tunnel};
use std::{
    fs,
    process::{Command, Output},
};

struct Cli {
    directory: tempfile::TempDir,
}

impl Cli {
    fn new() -> Self {
        let cli = Self {
            directory: tempfile::tempdir().unwrap(),
        };
        let ssh_config = cli.directory.path().join("empty_ssh_config");
        fs::write(&ssh_config, "").unwrap();
        cli.success(&[
            "server",
            "add",
            "dev",
            "--host",
            "127.0.0.1",
            "--ssh-config",
            ssh_config.to_str().unwrap(),
        ]);
        cli
    }

    fn output(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fwm"))
            .arg("--config-dir")
            .arg(self.directory.path())
            .arg("--json")
            .args(args)
            .output()
            .unwrap()
    }

    fn success(&self, args: &[&str]) -> Output {
        let output = self.output(args);
        assert!(
            output.status.success(),
            "fwm {args:?} failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn config(&self) -> Config {
        let output = self.success(&["config", "export"]);
        let config: Config = serde_json::from_slice(&output.stdout).unwrap();
        let persisted: Config =
            toml::from_str(&fs::read_to_string(self.config_path()).unwrap()).unwrap();
        assert_eq!(
            config, persisted,
            "IPC configuration must match durable configuration"
        );
        config
    }

    fn config_path(&self) -> std::path::PathBuf {
        self.directory.path().join("config.toml")
    }

    fn stop(&self) {
        self.success(&["daemon", "stop"]);
    }
}

impl Drop for Cli {
    fn drop(&mut self) {
        // Clean up even if an assertion fails before the explicit stop.
        let _ = self.output(&["daemon", "stop"]);
    }
}

fn rule<'a>(config: &'a Config, name: &str) -> &'a ForwardSpec {
    config
        .forwards
        .iter()
        .find(|rule| rule.name == name)
        .unwrap_or_else(|| panic!("missing rule {name}"))
}

fn assert_forward(config: &Config, name: &str, remote: bool, source: u16, target: &str) {
    let forward = rule(config, name);
    assert_eq!(forward.desired_state, DesiredState::Stopped);
    match (&forward.tunnel, remote) {
        (
            Tunnel::Local {
                listen,
                target: endpoint,
            },
            false,
        )
        | (
            Tunnel::Remote {
                listen,
                target: endpoint,
            },
            true,
        ) => {
            assert_eq!(listen.to_string(), format!("127.0.0.1:{source}"));
            assert_eq!(endpoint.to_string(), target);
        }
        _ => panic!("unexpected direction for {name}: {:?}", forward.tunnel),
    }
}

#[test]
fn same_port_shorthand_expands_atomically_and_survives_daemon_restart() {
    let cli = Cli::new();
    let initial_revision = cli.config().revision;
    cli.success(&[
        "add",
        "web",
        "--server",
        "dev",
        "--local",
        "--port",
        "3000",
        "--disabled",
    ]);
    let one = cli.config();
    assert_eq!(one.revision, initial_revision + 1);
    assert_eq!(one.forwards.len(), 1);
    assert_forward(&one, "web", false, 3000, "localhost:3000");

    cli.success(&[
        "add",
        "remote",
        "--server",
        "dev",
        "--remote",
        "--port",
        "8080,3000-3002,3001",
        "--disabled",
    ]);
    let batch = cli.config();
    assert_eq!(batch.revision, one.revision + 1);
    let generated_names: Vec<_> = batch
        .forwards
        .iter()
        .skip(1)
        .map(|rule| rule.name.as_str())
        .collect();
    assert_eq!(
        generated_names,
        ["remote-3000", "remote-3001", "remote-3002", "remote-8080"]
    );
    for port in [3000, 3001, 3002, 8080] {
        assert_forward(
            &batch,
            &format!("remote-{port}"),
            true,
            port,
            &format!("localhost:{port}"),
        );
    }

    cli.stop();
    cli.success(&["daemon", "start"]);
    assert_eq!(
        cli.config(),
        batch,
        "restart must preserve expanded rules and their stopped intent"
    );
    cli.stop();
}

#[test]
fn many_sources_target_one_port_and_single_rule_edits_preserve_direction() {
    let cli = Cli::new();
    let revision = cli.config().revision;
    cli.success(&[
        "add",
        "fan",
        "--server",
        "dev",
        "--local",
        "--src",
        "6000,5000-5002,5001",
        "--tgt",
        "3000",
        "--disabled",
    ]);
    let local = cli.config();
    assert_eq!(local.revision, revision + 1);
    assert_eq!(local.forwards.len(), 4);
    for port in [5000, 5001, 5002, 6000] {
        assert_forward(
            &local,
            &format!("fan-{port}"),
            false,
            port,
            "localhost:3000",
        );
    }

    cli.success(&[
        "add",
        "remote-fan",
        "--server",
        "dev",
        "--remote",
        "--src",
        "7000-7001",
        "--tgt",
        "3001",
        "--disabled",
    ]);
    let remote = cli.config();
    assert_eq!(remote.revision, local.revision + 1);
    for port in [7000, 7001] {
        assert_forward(
            &remote,
            &format!("remote-fan-{port}"),
            true,
            port,
            "localhost:3001",
        );
    }

    cli.success(&[
        "add",
        "legacy-local",
        "--server",
        "dev",
        "--local",
        "9000:remote.internal:80",
        "--disabled",
    ]);
    cli.success(&[
        "add",
        "legacy-remote",
        "--server",
        "dev",
        "--remote",
        "9100:127.0.0.1:9000",
        "--disabled",
    ]);
    let legacy = cli.config();
    assert_forward(&legacy, "legacy-local", false, 9000, "remote.internal:80");
    assert_forward(&legacy, "legacy-remote", true, 9100, "127.0.0.1:9000");

    cli.success(&["edit", "legacy-local", "--port", "9001"]);
    cli.success(&["edit", "legacy-remote", "--port", "9101"]);
    let edited = cli.config();
    assert_eq!(edited.revision, legacy.revision + 2);
    assert_forward(&edited, "legacy-local", false, 9001, "remote.internal:9001");
    assert_forward(&edited, "legacy-remote", true, 9101, "127.0.0.1:9101");
    assert_eq!(
        rule(&edited, "legacy-local").id,
        rule(&legacy, "legacy-local").id
    );
    assert_eq!(
        rule(&edited, "legacy-remote").id,
        rule(&legacy, "legacy-remote").id
    );
    cli.stop();
}

#[test]
fn invalid_or_conflicting_batches_never_change_config_or_revision() {
    let cli = Cli::new();
    cli.success(&[
        "add",
        "batch-3002",
        "--server",
        "dev",
        "--local",
        "--port",
        "9000",
        "--disabled",
    ]);
    let before = cli.config();
    let persisted_before = fs::read(cli.config_path()).unwrap();
    let invalid_commands: &[&[&str]] = &[
        // A conflict late in the expansion must not create either earlier rule.
        &[
            "add",
            "batch",
            "--server",
            "dev",
            "--local",
            "--port",
            "3000-3002",
            "--disabled",
        ],
        &[
            "add",
            "bad",
            "--server",
            "dev",
            "--local",
            "--port",
            "0",
            "--disabled",
        ],
        &[
            "add",
            "bad",
            "--server",
            "dev",
            "--remote",
            "--port",
            "65536",
            "--disabled",
        ],
        &[
            "add",
            "bad",
            "--server",
            "dev",
            "--local",
            "--port",
            "3002-3000",
            "--disabled",
        ],
        &[
            "add",
            "bad",
            "--server",
            "dev",
            "--local",
            "--port",
            "3000,,3001",
            "--disabled",
        ],
        &[
            "add",
            "bad",
            "--server",
            "dev",
            "--local",
            "--port",
            "1-65535",
            "--disabled",
        ],
        &[
            "add",
            "bad",
            "--server",
            "dev",
            "--local",
            "--src",
            "3000-3001",
            "--disabled",
        ],
        &[
            "add",
            "bad",
            "--server",
            "dev",
            "--remote",
            "--src",
            "3000-3001",
            "--tgt",
            "4000,4001",
            "--disabled",
        ],
        &[
            "add",
            "bad",
            "--server",
            "dev",
            "--local",
            "--port",
            "3000",
            "--src",
            "4000",
            "--tgt",
            "5000",
            "--disabled",
        ],
        &[
            "add",
            "bad",
            "--server",
            "dev",
            "--local",
            "3000:localhost:3000",
            "--port",
            "4000",
            "--disabled",
        ],
        &[
            "add",
            "bad",
            "--server",
            "dev",
            "--dynamic",
            "1080",
            "--port",
            "4000",
            "--disabled",
        ],
        &["edit", "batch-3002", "--port", "4000-4001"],
        &["edit", "batch-3002", "--src", "4000-4001", "--tgt", "5000"],
    ];
    for args in invalid_commands {
        let output = cli.output(args);
        assert!(
            !output.status.success(),
            "invalid command unexpectedly succeeded: {args:?}"
        );
        assert_eq!(
            cli.config(),
            before,
            "failed command changed configuration: {args:?}"
        );
        assert_eq!(
            fs::read(cli.config_path()).unwrap(),
            persisted_before,
            "failed command rewrote durable configuration: {args:?}"
        );
    }
    cli.stop();
}
