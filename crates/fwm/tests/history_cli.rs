//! End-to-end history coverage through the real CLI, daemon, and saved journal.
//! All forwards are disabled and each test owns a private configuration directory.

use std::{
    collections::HashSet,
    fs,
    process::{Command, Output},
};

use fwm_core::{history::HistoryEntry, model::Config};
use serde_json::Value;

struct Cli {
    directory: tempfile::TempDir,
}

impl Cli {
    fn new() -> Self {
        let cli = Self {
            directory: tempfile::tempdir().unwrap(),
        };
        fs::write(cli.directory.path().join("ssh_config"),
            "Host history-host\n HostName 127.0.0.1\n User fixture\n Port 2222\n IdentityAgent none\n").unwrap();
        cli
    }

    fn output(&self, args: &[&str], json: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fwm"));
        command.arg("--config-dir").arg(self.directory.path());
        if json {
            command.arg("--json");
        }
        command.args(args).output().unwrap()
    }

    fn success(&self, args: &[&str], json: bool) -> Output {
        let output = self.output(args, json);
        assert!(
            output.status.success(),
            "fwm {args:?} failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.success(args, true);
        // from_slice rejects trailing data, so multiple top-level JSON results
        // cannot accidentally pass a test that only examined the first object.
        let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "fwm {args:?} did not produce exactly one JSON result: {error}\n{}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
        assert!(value.is_object());
        value
    }

    fn add_batch(&self, ports: &str) -> Config {
        self.json(&[
            "add",
            "--name",
            "batch",
            "--server",
            "history-host",
            "--ssh-config",
            self.directory.path().join("ssh_config").to_str().unwrap(),
            "--local",
            "--port",
            ports,
            "--disabled",
        ]);
        serde_json::from_value(self.json(&["config", "export"])).unwrap()
    }

    fn logs(&self, args: &[&str]) -> Vec<HistoryEntry> {
        let mut command = vec!["logs"];
        command.extend_from_slice(args);
        let value = self.json(&command);
        assert_eq!(
            value["warnings"],
            serde_json::json!([]),
            "unexpected history warning: {value}"
        );
        serde_json::from_value(value["events"].clone()).unwrap()
    }

    fn running(&self) -> bool {
        self.json(&["daemon", "status"])["daemon_running"]
            .as_bool()
            .unwrap()
    }
}

impl Drop for Cli {
    fn drop(&mut self) {
        let _ = self.output(&["daemon", "stop"], true);
    }
}

fn keys(entries: &[HistoryEntry]) -> HashSet<(String, u64)> {
    entries
        .iter()
        .map(|entry| (entry.daemon_instance_id.clone(), entry.event.sequence))
        .collect()
}

#[test]
fn historical_names_groups_and_servers_survive_rename_restart_delete_and_offline_queries() {
    let cli = Cli::new();
    let created = cli.add_batch("13000-13002");
    assert_eq!(created.forwards.len(), 3);
    assert!(
        created
            .forwards
            .iter()
            .all(|rule| rule.group.as_deref() == Some("batch"))
    );
    let id = created.forward("batch-13000").unwrap().id.clone();
    let original = cli.logs(&["batch-13000", "--tail", "1000"]);
    assert!(
        !original.is_empty(),
        "creation must write a rule-labeled history event"
    );
    assert!(
        original
            .iter()
            .all(|entry| entry.event.forward_id.as_deref() == Some(id.as_str()))
    );
    assert!(
        original
            .iter()
            .all(|entry| entry.forward_name.as_deref() == Some("batch-13000"))
    );

    cli.json(&["edit", "batch-13000", "--rename", "renamed-rule"]);
    let old_name = cli.logs(&["batch-13000", "--tail", "1000"]);
    let new_name = cli.logs(&["renamed-rule", "--tail", "1000"]);
    assert_eq!(
        keys(&old_name),
        keys(&new_name),
        "old and new names must resolve the same stable rule ID"
    );
    assert!(keys(&original).is_subset(&keys(&old_name)));
    assert!(
        old_name
            .iter()
            .any(|entry| entry.forward_name.as_deref() == Some("renamed-rule"))
    );
    assert!(
        old_name
            .iter()
            .any(|entry| entry.forward_name.as_deref() == Some("batch-13000"))
    );

    cli.json(&["daemon", "restart"]);
    let after_restart = cli.logs(&["batch-13000", "--tail", "1000"]);
    assert!(
        keys(&old_name).is_subset(&keys(&after_restart)),
        "restart discarded persisted history"
    );
    cli.json(&["remove", "renamed-rule"]);
    let deleted = cli.logs(&["renamed-rule", "--tail", "1000"]);
    assert!(
        deleted.len() > after_restart.len(),
        "removal must keep a labeled event for the deleted rule"
    );
    assert!(
        deleted
            .iter()
            .all(|entry| entry.event.forward_id.as_deref() == Some(id.as_str()))
    );
    assert!(
        deleted
            .iter()
            .map(|entry| entry.daemon_instance_id.as_str())
            .collect::<HashSet<_>>()
            .len()
            >= 2,
        "history must preserve both daemon instances, even though sequences restart"
    );

    cli.json(&["remove", "--group", "batch"]);
    let remaining: Config = serde_json::from_value(cli.json(&["config", "export"])).unwrap();
    assert!(remaining.forwards.is_empty());
    cli.json(&["server", "remove", "history-host"]);
    let by_group = cli.logs(&["--group", "batch", "--tail", "1000"]);
    let by_server = cli.logs(&["--server", "history-host", "--tail", "1000"]);
    assert!(
        by_group
            .iter()
            .all(|entry| entry.group.as_deref() == Some("batch"))
    );
    let server_rule_events: Vec<_> = by_server
        .iter()
        .filter(|entry| entry.event.forward_id.is_some())
        .cloned()
        .collect();
    assert_eq!(keys(&by_group), keys(&server_rule_events));
    assert!(
        by_server
            .iter()
            .any(|entry| entry.event.forward_id.is_none()
                && entry.server_name.as_deref() == Some("history-host")),
        "server queries must also include server creation/removal events"
    );
    let historical_ids: HashSet<_> = by_group
        .iter()
        .filter_map(|entry| entry.event.forward_id.as_deref())
        .collect();
    assert_eq!(
        historical_ids.len(),
        3,
        "deleted batch members must remain discoverable"
    );
    assert_eq!(
        keys(&cli.logs(&["batch", "--tail", "1000"])),
        keys(&by_group),
        "positional group query should match --group"
    );

    cli.json(&["daemon", "stop"]);
    assert!(!cli.running());
    let config_before = fs::read(cli.directory.path().join("config.toml")).unwrap();
    let history_path = cli.directory.path().join("state/events.jsonl");
    let journal_before = fs::read(&history_path).unwrap();
    assert_eq!(
        keys(&cli.logs(&["batch-13000", "--tail", "1000"])),
        keys(&deleted)
    );
    assert_eq!(
        keys(&cli.logs(&["--group", "batch", "--tail", "1000"])),
        keys(&by_group)
    );
    assert_eq!(
        keys(&cli.logs(&["--server", "history-host", "--tail", "1000"])),
        keys(&by_server)
    );
    assert!(!cli.running(), "offline log reads must not start a daemon");
    assert_eq!(
        fs::read(cli.directory.path().join("config.toml")).unwrap(),
        config_before
    );
    assert_eq!(
        fs::read(history_path).unwrap(),
        journal_before,
        "reading history must not append to it"
    );
}

#[test]
fn tail_is_exact_json_is_one_object_and_human_logs_use_utc_dates() {
    let cli = Cli::new();
    cli.add_batch("14000-14109");
    // Freeze this fixture before comparing tail results so asynchronous engine
    // events cannot race two independent public CLI snapshots.
    cli.json(&["daemon", "stop"]);
    assert!(!cli.running());
    let all = cli.logs(&["--group", "batch", "--tail", "1000"]);
    assert!(
        all.len() >= 110,
        "every batch member needs durable scope labels"
    );
    assert_eq!(
        cli.logs(&["--group", "batch"]).len(),
        100,
        "default tail must be 100"
    );
    let recent = cli.logs(&["--group", "batch", "--tail", "3"]);
    assert_eq!(recent.len(), 3);
    assert_eq!(keys(&recent), keys(&all[all.len() - 3..]));
    assert!(cli.logs(&["--group", "batch", "--tail", "0"]).is_empty());
    let human = cli.success(&["logs", "--group", "batch", "--tail", "1"], false);
    let text = String::from_utf8(human.stdout).unwrap();
    let line = text.trim();
    assert!(line.len() > 30, "expected a dated event, got {line:?}");
    assert_eq!(&line[4..5], "-");
    assert_eq!(&line[7..8], "-");
    assert_eq!(&line[10..11], " ");
    assert!(line[..4].parse::<u16>().unwrap() >= 2020);
    assert!(
        line.contains(" UTC  batch-"),
        "timestamp must be human-readable UTC: {line}"
    );
    assert!(
        line.contains("[history-host / batch]"),
        "human output must retain server/group labels: {line}"
    );
    assert!(!cli.running());
}
