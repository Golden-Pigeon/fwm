use fwm_core::{
    model::{
        Config, ConnectionMode, DesiredState, ForwardSpec, RemoteCleanup, ServerProfile, Tunnel,
    },
    paths::Paths,
    store::Store,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

struct Fixture {
    directory: tempfile::TempDir,
    paths: Paths,
    config: Config,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(directory.path().join("config with spaces"))).unwrap();
        let servers = ["dev", "production"]
            .into_iter()
            .map(|name| {
                let mut server = ServerProfile::new(name);
                server.id = format!("server-{name}");
                server.host = Some("127.0.0.1".into());
                server
            })
            .collect();
        let forwards = [
            ("web", 31001, Some("apps")),
            ("worker", 31002, Some("apps")),
            ("db", 31003, None),
        ]
        .into_iter()
        .map(|(name, port, group)| ForwardSpec {
            id: format!("rule-{name}"),
            name: name.into(),
            group: group.map(str::to_owned),
            server_id: "server-dev".into(),
            tunnel: Tunnel::Local {
                listen: ([127, 0, 0, 1], port).into(),
                target: "localhost:8080".parse().unwrap(),
            },
            desired_state: DesiredState::Stopped,
            connection_mode: ConnectionMode::Shared,
            remote_cleanup: RemoteCleanup::Off,
        })
        .collect();
        let config = Config {
            servers,
            forwards,
            ..Config::default()
        };
        Store::new(paths.clone()).initialize(&config).unwrap();
        Self {
            directory,
            paths,
            config,
        }
    }

    fn complete(&self, arguments: &[&str]) -> Vec<String> {
        let mut words = vec![
            "fwm",
            "--config-dir",
            self.paths.config_dir.to_str().unwrap(),
        ];
        words.extend_from_slice(arguments);
        self.complete_at(&words, words.len() - 1)
    }

    fn complete_at(&self, words: &[&str], cursor: usize) -> Vec<String> {
        let before = contents(self.directory.path());
        let output = Command::new(env!("CARGO_BIN_EXE_fwm"))
            .current_dir(self.directory.path())
            .env("FWM_COMPLETE", "bash")
            .env("TOKIO_WORKER_THREADS", "0")
            .env("_CLAP_COMPLETE_INDEX", cursor.to_string())
            .env("_CLAP_IFS", "\n")
            .arg("--")
            .args(words)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{words:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "completion must be quiet: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            contents(self.directory.path()),
            before,
            "completion must not write configuration, locks, sockets, or state"
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn save(&self) {
        Store::new(self.paths.clone()).commit(&self.config).unwrap();
    }
}

fn contents(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(root: &Path, path: &Path, entries: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            let relative = path.strip_prefix(root).unwrap().to_owned();
            if path.is_dir() {
                entries.insert(relative, None);
                visit(root, &path, entries);
            } else {
                entries.insert(relative, Some(fs::read(path).unwrap()));
            }
        }
    }
    let mut entries = BTreeMap::new();
    visit(root, root, &mut entries);
    entries
}

fn assert_contains(actual: &[String], expected: &[&str]) {
    for candidate in expected {
        assert!(
            actual.iter().any(|value| value == candidate),
            "missing {candidate:?} in {actual:?}"
        );
    }
}

#[test]
fn saved_server_names_and_ids_complete_in_all_selector_positions() {
    let fixture = Fixture::new();
    for action in ["edit", "remove", "trust", "check"] {
        assert_contains(
            &fixture.complete(&["server", action, ""]),
            &["dev", "production", "server-dev", "server-production"],
        );
    }
    for action in [
        "add", "edit", "up", "down", "retry", "restart", "remove", "status", "logs", "doctor",
    ] {
        assert_eq!(
            fixture.complete(&[action, "--server", "pro"]),
            ["production"]
        );
        assert_eq!(
            fixture.complete(&[action, "--server=pro"]),
            ["--server=production"]
        );
    }
}

#[test]
fn existing_rule_names_ids_and_groups_complete_without_duplicate_groups() {
    let fixture = Fixture::new();
    for action in [
        "edit", "up", "down", "retry", "restart", "remove", "status", "logs",
    ] {
        let candidates = fixture.complete(&[action, ""]);
        assert_contains(
            &candidates,
            &[
                "web",
                "worker",
                "db",
                "rule-web",
                "rule-worker",
                "rule-db",
                "apps",
            ],
        );
        assert_eq!(
            candidates
                .iter()
                .filter(|candidate| candidate.as_str() == "apps")
                .count(),
            1
        );
        assert_eq!(fixture.complete(&[action, "wo"]), ["worker"]);
    }
    for action in [
        "add", "edit", "up", "down", "retry", "restart", "remove", "status", "logs",
    ] {
        assert_eq!(fixture.complete(&[action, "--group", ""]), ["apps"]);
        assert_eq!(fixture.complete(&[action, "--group=ap"]), ["--group=apps"]);
    }
}

#[test]
fn newly_created_names_and_renames_remain_free_form() {
    let fixture = Fixture::new();
    for arguments in [
        vec!["add", "--name", ""],
        vec!["add", "we"],
        vec!["server", "add", "de"],
        vec!["server", "add", "new", "--host", "de"],
        vec!["edit", "web", "--rename", ""],
        vec!["server", "edit", "dev", "--rename", ""],
    ] {
        assert!(
            fixture.complete(&arguments).is_empty(),
            "unexpected candidates for {arguments:?}"
        );
    }
}

#[test]
fn configuration_changes_are_visible_on_the_next_completion() {
    let mut fixture = Fixture::new();
    assert_eq!(fixture.complete(&["up", "we"]), ["web"]);
    fixture.config.servers[0].name = "development".into();
    fixture.config.forwards[0].name = "website".into();
    fixture.config.forwards[0].group = Some("frontend".into());
    fixture.config.forwards.remove(1);
    fixture.save();
    assert_eq!(fixture.complete(&["up", "we"]), ["website"]);
    assert_eq!(fixture.complete(&["up", "wo"]), Vec::<String>::new());
    assert_eq!(
        fixture.complete(&["up", "--server", "dev"]),
        ["development"]
    );
    assert_eq!(fixture.complete(&["up", "--group", ""]), ["frontend"]);
    fixture.config.forwards.clear();
    fixture.config.servers.remove(0);
    fixture.save();
    assert!(fixture.complete(&["up", "we"]).is_empty());
    assert!(fixture.complete(&["up", "--group", ""]).is_empty());
    assert!(fixture.complete(&["up", "--server", "dev"]).is_empty());
}

#[test]
fn config_directory_is_honored_at_global_and_nested_positions() {
    let fixture = Fixture::new();
    let directory = fixture.paths.config_dir.to_str().unwrap();
    let assignment = format!("--config-dir={directory}");
    for words in [
        vec!["fwm", "--config-dir", directory, "server", "check", "de"],
        vec!["fwm", "server", "--config-dir", directory, "check", "de"],
        vec!["fwm", "server", "check", "--config-dir", directory, "de"],
        vec!["fwm", &assignment, "server", "check", "de"],
        vec!["fwm", "server", &assignment, "check", "de"],
        vec!["fwm", "server", "check", &assignment, "de"],
    ] {
        assert_eq!(fixture.complete_at(&words, words.len() - 1), ["dev"]);
    }
}

#[test]
fn cursor_can_precede_other_words_or_represent_an_empty_word() {
    let fixture = Fixture::new();
    let directory = fixture.paths.config_dir.to_str().unwrap();
    let words = ["fwm", "up", "we", "--config-dir", directory];
    assert_eq!(fixture.complete_at(&words, 2), ["web"]);
    let words = ["fwm", "--config-dir", directory, "up", "--group", ""];
    assert_eq!(fixture.complete_at(&words, words.len() - 1), ["apps"]);
}

#[test]
fn persisted_ids_with_control_characters_cannot_inject_completion_lines() {
    let mut fixture = Fixture::new();
    fixture.config.forwards[0].id = "rule-web\ninjected-option".into();
    fixture.config.forwards[1].id = "rule-worker\tannotation".into();
    fixture.config.servers[1].id = "server-production\rterminal-return".into();
    fixture.save();
    let candidates = fixture.complete(&["up", ""]);
    assert_contains(&candidates, &["web", "worker", "db", "apps"]);
    assert!(
        !candidates
            .iter()
            .any(|candidate| candidate.contains("injected")
                || candidate.contains("annotation")
                || candidate.contains("rule-web"))
    );
    assert!(
        fixture
            .complete(&["up", "--server", "server-pro"])
            .is_empty()
    );
    assert_eq!(fixture.complete(&["up", "--server", "pro"]), ["production"]);
}

#[test]
fn unapplied_or_broken_candidate_does_not_replace_committed_selectors() {
    let mut fixture = Fixture::new();
    fixture.config.forwards[0].name = "pending-web".into();
    fs::write(
        &fixture.paths.config_file,
        toml::to_string(&fixture.config).unwrap(),
    )
    .unwrap();
    assert_eq!(fixture.complete(&["up", "we"]), ["web"]);
    assert!(fixture.complete(&["up", "pending"]).is_empty());
    fs::write(&fixture.paths.config_file, "this is not valid toml = [").unwrap();
    assert_eq!(fixture.complete(&["up", "we"]), ["web"]);
    fs::write(fixture.paths.state_dir.join("applied.toml"), "broken = [").unwrap();
    assert!(fixture.complete(&["up", "we"]).is_empty());
    assert!(fixture.complete(&["up", "--server", "de"]).is_empty());
}

#[test]
fn missing_configuration_is_not_created_by_completion() {
    let fixture = Fixture::new();
    let missing = fixture.directory.path().join("not-created");
    let words = [
        "fwm",
        "--config-dir",
        missing.to_str().unwrap(),
        "up",
        "anything",
    ];
    assert!(fixture.complete_at(&words, words.len() - 1).is_empty());
    assert!(!missing.exists());
}

#[test]
fn command_options_and_enums_complete_in_context() {
    let fixture = Fixture::new();
    assert_eq!(fixture.complete(&["ser"]), ["server", "service"]);
    assert_contains(
        &fixture.complete(&["server", ""]),
        &["add", "edit", "list", "remove", "trust", "check"],
    );
    assert_eq!(
        fixture.complete(&["add", "--connection-mode", "d"]),
        ["dedicated"]
    );
    assert_eq!(
        fixture.complete(&["edit", "web", "--remote-cleanup=o"]),
        ["--remote-cleanup=off"]
    );
    assert_contains(
        &fixture.complete(&["add", "--s"]),
        &["--server", "--ssh-config", "--src"],
    );
    let top_level = fixture.complete(&[""]);
    assert!(top_level.iter().all(|candidate| candidate != "__complete"));
    assert_contains(&top_level, &["completions"]);
    assert_contains(&fixture.complete(&["completions", ""]), &["bash", "zsh"]);
}

#[test]
fn equals_assignments_and_flags_do_not_consume_the_selector_position() {
    let fixture = Fixture::new();
    for arguments in [
        vec!["up", "--json", "we"],
        vec!["up", "--timeout", "5s", "we"],
        vec!["up", "--timeout=5s", "we"],
        vec!["edit", "--local", "we"],
        vec!["edit", "-L", "we"],
        vec!["edit", "--local=32001:localhost:8080", "we"],
        vec!["edit", "-L", "32001:localhost:8080", "we"],
        vec!["edit", "-R", "32001:localhost:8080", "we"],
    ] {
        assert_eq!(fixture.complete(&arguments), ["web"], "{arguments:?}");
    }
}

#[test]
fn filesystem_candidates_preserve_spaces_and_directory_suffixes() {
    let fixture = Fixture::new();
    fs::write(fixture.directory.path().join("ssh config"), "Host *\n").unwrap();
    fs::create_dir(fixture.directory.path().join("ssh directory")).unwrap();
    let candidates: BTreeSet<_> = fixture
        .complete(&["add", "--ssh-config", "ssh "])
        .into_iter()
        .collect();
    assert_eq!(
        candidates,
        BTreeSet::from(["ssh config".into(), "ssh directory/".into()])
    );
    assert_eq!(
        fixture.complete(&["add", "--ssh-config=ssh c"]),
        ["--ssh-config=ssh config"]
    );
}

#[test]
fn scripts_are_generated_without_reading_or_creating_configuration() {
    let directory = tempfile::tempdir().unwrap();
    for shell in ["bash", "zsh"] {
        let config = directory.path().join(format!("unused-{shell}"));
        let output = Command::new(env!("CARGO_BIN_EXE_fwm"))
            .arg("--config-dir")
            .arg(&config)
            .args(["completions", shell])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        let script = String::from_utf8(output.stdout).unwrap();
        assert!(script.contains("FWM_COMPLETE"));
        assert!(!script.contains("__complete"));
        assert!(!config.exists());
        #[cfg(unix)]
        {
            let path = directory.path().join(format!("completion.{shell}"));
            fs::write(&path, &script).unwrap();
            match Command::new(shell).arg("-n").arg(&path).output() {
                Ok(check) => assert!(
                    check.status.success(),
                    "{shell} syntax: {}",
                    String::from_utf8_lossy(&check.stderr)
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("cannot check {shell} script: {error}"),
            }
        }
    }
}
