//! Exercise first-use `add` through the actual binary, IPC, and durable config.
//! Disabled rules make every case independent of SSH connectivity and listeners.

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

use fwm_core::model::{Config, DesiredState, ForwardSpec, Tunnel};

struct Cli {
    directory: tempfile::TempDir,
    ssh_config: PathBuf,
}

impl Cli {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let ssh_config = directory.path().join("ssh_config");
        fs::write(
            &ssh_config,
            "Host example-cluster second-school\n HostName 127.0.0.1\n User fixture\n Port 2222\n IdentityAgent none\n",
        )
        .unwrap();
        let cli = Self {
            directory,
            ssh_config,
        };
        // Establish the daemon's initial empty file before taking mutation
        // snapshots, so startup bootstrap is not mistaken for a partial add.
        cli.success(&["daemon", "start"]);
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

    fn add(&self, args: &[&str]) -> Output {
        let mut command = vec!["add"];
        command.extend_from_slice(args);
        command.extend_from_slice(&[
            "--ssh-config",
            self.ssh_config.to_str().unwrap(),
            "--disabled",
        ]);
        self.output(&command)
    }

    fn success(&self, args: &[&str]) -> Output {
        let output = self.output(args);
        assert_success(args, &output);
        output
    }

    fn add_success(&self, args: &[&str]) {
        assert_success(args, &self.add(args));
    }

    fn config(&self) -> Config {
        let output = self.success(&["config", "export"]);
        let config: Config = serde_json::from_slice(&output.stdout).unwrap();
        if let Some(bytes) = self.persisted() {
            let persisted: Config = toml::from_str(std::str::from_utf8(&bytes).unwrap()).unwrap();
            assert_eq!(config, persisted, "IPC and durable config disagree");
        }
        config
    }

    fn persisted(&self) -> Option<Vec<u8>> {
        match fs::read(self.directory.path().join("config.toml")) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("cannot read durable configuration: {error}"),
        }
    }

    fn assert_unchanged(&self, before: &Config, persisted: &Option<Vec<u8>>) {
        assert_eq!(&self.config(), before, "failed add changed configuration");
        assert_eq!(
            &self.persisted(),
            persisted,
            "failed add rewrote config.toml"
        );
    }
}

impl Drop for Cli {
    fn drop(&mut self) {
        // The temporary directory must outlive daemon shutdown, including when
        // a failed assertion unwinds. No service registration is performed.
        let _ = self.output(&["daemon", "stop"]);
    }
}

fn assert_success(args: &[&str], output: &Output) {
    assert!(
        output.status.success(),
        "fwm {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn failure_text(output: &Output) -> String {
    assert!(
        output.status.code().is_some(),
        "CLI terminated without exit code"
    );
    assert!(
        !output.status.success(),
        "invalid add unexpectedly succeeded"
    );
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn rule<'a>(config: &'a Config, name: &str) -> &'a ForwardSpec {
    let rule = config
        .forward(name)
        .unwrap_or_else(|| panic!("missing rule {name}: {:?}", config.forwards));
    assert_eq!(rule.desired_state, DesiredState::Stopped);
    rule
}

fn assert_tcp(config: &Config, name: &str, remote: bool, source: u16, target_port: u16) {
    let rule = rule(config, name);
    match (&rule.tunnel, remote) {
        (Tunnel::Remote { listen, target }, true) | (Tunnel::Local { listen, target }, false) => {
            assert_eq!(listen.to_string(), format!("127.0.0.1:{source}"));
            assert_eq!(target.to_string(), format!("localhost:{target_port}"));
        }
        _ => panic!("wrong forwarding direction for {name}: {:?}", rule.tunnel),
    }
}

fn assert_random_word(name: &str) {
    assert!((3..=8).contains(&name.len()), "not a short word: {name}");
    assert!(
        name.bytes().all(|letter| letter.is_ascii_lowercase()),
        "not a lowercase English word: {name}"
    );
}

fn automatic_rule(config: &Config, source: u16) -> &ForwardSpec {
    let rule = config
        .forwards
        .iter()
        .find(|rule| rule.tunnel.listen().port() == source)
        .unwrap_or_else(|| panic!("missing rule for port {source}"));
    assert_random_word(&rule.name);
    assert_eq!(rule.desired_state, DesiredState::Stopped);
    rule
}

#[test]
fn first_add_accepts_an_ssh_alias_without_name_or_preregistration_atomically() {
    let cli = Cli::new();
    let before = cli.config();
    assert!(before.servers.is_empty());
    cli.add_success(&[
        "--server",
        "example-cluster",
        "--remote",
        "--src",
        "12222",
        "--tgt",
        "22",
    ]);
    let config = cli.config();
    assert_eq!(config.revision, before.revision + 1);
    assert_eq!(config.servers.len(), 1);
    assert_eq!(config.forwards.len(), 1);
    assert!(cli.persisted().is_some(), "successful add must be durable");
    let server = &config.servers[0];
    assert_eq!(server.name, "example-cluster");
    assert_eq!(server.ssh_alias.as_deref(), Some("example-cluster"));
    assert_eq!(
        server.ssh_config.as_ref(),
        Some(&cli.ssh_config.canonicalize().unwrap())
    );
    let first = automatic_rule(&config, 12222);
    assert_tcp(&config, &first.name, true, 12222, 22);
    assert!(first.group.is_none());
    assert_eq!(config.forwards[0].server_id, server.id);

    cli.add_success(&[
        "--server",
        "example-cluster",
        "--remote",
        "--src",
        "12223",
        "--tgt",
        "22",
    ]);
    let second = cli.config();
    assert_eq!(second.revision, config.revision + 1);
    assert_eq!(
        second.servers, config.servers,
        "reuse the existing server identity"
    );
    assert_eq!(second.forwards.len(), 2);
    let added = automatic_rule(&second, 12223);
    assert_tcp(&second, &added.name, true, 12223, 22);
    assert_ne!(added.name, first.name);
    assert!(added.group.is_none());
    assert_eq!(second.forward(&first.id), Some(first));
    assert!(
        second
            .forwards
            .iter()
            .all(|rule| rule.server_id == server.id)
    );

    cli.success(&["daemon", "stop"]);
    cli.success(&["daemon", "start"]);
    assert_eq!(
        cli.config(),
        second,
        "restart must preserve server and rules"
    );
}

#[test]
fn positional_and_named_rule_names_work_around_direction_flags() {
    let cli = Cli::new();
    let cases: &[(&str, &[&str], u16)] = &[
        (
            "cluster-ssh-mac",
            &[
                "--server",
                "example-cluster",
                "--remote",
                "cluster-ssh-mac",
                "--src",
                "12222",
                "--tgt",
                "22",
            ],
            12222,
        ),
        (
            "leading-name",
            &[
                "leading-name",
                "--server",
                "example-cluster",
                "--remote",
                "--src",
                "12223",
                "--tgt",
                "22",
            ],
            12223,
        ),
        (
            "trailing-name",
            &[
                "--remote",
                "--src",
                "12224",
                "--tgt",
                "22",
                "--server",
                "example-cluster",
                "trailing-name",
            ],
            12224,
        ),
        (
            "flag-name",
            &[
                "--server",
                "example-cluster",
                "--remote",
                "--src",
                "12225",
                "--tgt",
                "22",
                "--name",
                "flag-name",
            ],
            12225,
        ),
    ];
    let before = cli.config();
    for (index, (name, args, source)) in cases.iter().enumerate() {
        cli.add_success(args);
        let config = cli.config();
        assert_eq!(config.revision, before.revision + index as u64 + 1);
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.forwards.len(), index + 1);
        assert_tcp(&config, name, true, *source, 22);
    }
}

#[test]
fn automatic_names_are_short_unique_words_and_explicit_batches_keep_name_prefix() {
    let cli = Cli::new();
    cli.add_success(&["--server", "example-cluster", "--remote", "--port", "12000"]);
    let before = cli.config();
    // An existing profile needs no SSH config override, but --server remains
    // required even when there is only one profile.
    cli.success(&[
        "add",
        "--server",
        "example-cluster",
        "--local",
        "--port",
        "3000-3001",
        "--disabled",
    ]);
    cli.success(&[
        "add",
        "--server",
        "example-cluster",
        "--dynamic",
        "1080",
        "--disabled",
    ]);
    cli.success(&[
        "add",
        "fan",
        "--server",
        "example-cluster",
        "--remote",
        "--src",
        "13000-13001",
        "--tgt",
        "22",
        "--disabled",
    ]);
    let config = cli.config();
    assert_eq!(config.revision, before.revision + 3);
    assert_eq!(config.servers, before.servers);
    assert_eq!(config.forwards.len(), 6);
    let first_local = automatic_rule(&config, 3000);
    let second_local = automatic_rule(&config, 3001);
    assert_tcp(&config, &first_local.name, false, 3000, 3000);
    assert_tcp(&config, &second_local.name, false, 3001, 3001);
    let group = first_local.group.as_deref().unwrap();
    assert_random_word(group);
    assert_eq!(second_local.group.as_deref(), Some(group));
    assert_eq!(config.select_group_forwards(group).unwrap().len(), 2);
    let dynamic = automatic_rule(&config, 1080);
    assert!(matches!(dynamic.tunnel, Tunnel::Dynamic { .. }));
    assert_eq!(dynamic.tunnel.listen().to_string(), "127.0.0.1:1080");
    assert!(dynamic.group.is_none());
    automatic_rule(&config, 12000);
    let mut names = std::collections::HashSet::new();
    for rule in &config.forwards {
        assert!(names.insert(&rule.name), "duplicate name: {}", rule.name);
        assert_ne!(rule.name, group, "a rule name must not shadow its group");
        assert!(config.forwards.iter().all(|other| other.id != rule.name));
    }
    assert_tcp(&config, "fan-13000", true, 13000, 22);
    assert_tcp(&config, "fan-13001", true, 13001, 22);
    assert_eq!(config.select_group_forwards("fan").unwrap().len(), 2);
    assert!(
        config
            .forwards
            .iter()
            .all(|rule| rule.server_id == config.servers[0].id)
    );
}

#[test]
fn server_is_required_with_zero_one_or_many_profiles_without_mutating_configuration() {
    let cli = Cli::new();
    let empty = cli.config();
    let empty_bytes = cli.persisted();
    let output = cli.output(&[
        "add",
        "--remote",
        "--src",
        "12222",
        "--tgt",
        "22",
        "--disabled",
    ]);
    let message = failure_text(&output);
    assert!(
        message.contains("--server"),
        "missing actionable server guidance: {message}"
    );
    cli.assert_unchanged(&empty, &empty_bytes);

    cli.add_success(&["--server", "example-cluster", "--remote", "--port", "12000"]);
    let single = cli.config();
    assert_eq!(single.servers.len(), 1);
    let single_bytes = cli.persisted();
    let output = cli.output(&["add", "--local", "--port", "3000", "--disabled"]);
    let message = failure_text(&output);
    assert!(
        message.contains("--server"),
        "missing required server guidance: {message}"
    );
    cli.assert_unchanged(&single, &single_bytes);

    cli.add_success(&["--server", "second-school", "--remote", "--port", "12001"]);
    let ambiguous = cli.config();
    assert_eq!(ambiguous.servers.len(), 2);
    let bytes = cli.persisted();
    let output = cli.output(&["add", "--local", "--port", "3000", "--disabled"]);
    let message = failure_text(&output);
    assert!(
        message.contains("--server"),
        "missing actionable server guidance: {message}"
    );
    cli.assert_unchanged(&ambiguous, &bytes);
}

#[test]
fn invalid_or_conflicting_names_do_not_leave_an_empty_automatic_server() {
    let cli = Cli::new();
    let before = cli.config();
    let persisted = cli.persisted();
    let invalid: &[&[&str]] = &[
        &[
            "bad/name",
            "--server",
            "example-cluster",
            "--remote",
            "--src",
            "12222",
            "--tgt",
            "22",
        ],
        &[
            "--name",
            "bad name",
            "--server",
            "example-cluster",
            "--remote",
            "--src",
            "12222",
            "--tgt",
            "22",
        ],
        &[
            "positional",
            "--name",
            "flagged",
            "--server",
            "example-cluster",
            "--remote",
            "--src",
            "12222",
            "--tgt",
            "22",
        ],
        &[
            "--remote",
            "positional",
            "--name",
            "flagged",
            "--server",
            "example-cluster",
            "--src",
            "12222",
            "--tgt",
            "22",
        ],
    ];
    for args in invalid {
        failure_text(&cli.add(args));
        cli.assert_unchanged(&before, &persisted);
    }

    // An existing rule name must also prevent a new alias profile from being
    // committed as a partial side effect of the rejected add operation.
    cli.add_success(&[
        "existing",
        "--server",
        "example-cluster",
        "--remote",
        "--port",
        "12000",
    ]);
    let before = cli.config();
    let persisted = cli.persisted();
    failure_text(&cli.add(&[
        "existing",
        "--server",
        "second-school",
        "--remote",
        "--port",
        "12001",
    ]));
    cli.assert_unchanged(&before, &persisted);
}
