//! Real CLI streaming loops over private state. No rule in these tests runs an
//! SSH connection. Unix-only signal assertions exercise graceful Ctrl-C exits.
#![cfg(unix)]

#[path = "support/cli_stream.rs"]
mod support;

use fwm_core::{
    history::{self, HistoryEntry, MAX_LOG_BYTES},
    model::{Config, EngineEvent},
};
use serde_json::Value;
use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
};
use support::{Stream, cli};

struct Fixture {
    directory: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        Self {
            directory: tempfile::tempdir().unwrap(),
        }
    }
    fn run(&self, args: &[&str]) -> Value {
        cli(self.directory.path(), args)
    }
    fn stream(&self, args: &[&str]) -> Stream {
        Stream::start(self.directory.path(), args)
    }
    fn add(&self, name: &str, port: &str, group: &str) {
        self.run(&[
            "add",
            "--server",
            "stream-fixture",
            "--local",
            "--port",
            port,
            "--name",
            name,
            "--group",
            group,
            "--disabled",
        ]);
    }
    fn config(&self) -> Config {
        serde_json::from_value(self.run(&["config", "export"])).unwrap()
    }
    fn journal(&self) -> PathBuf {
        self.directory.path().join("state/events.jsonl")
    }
    fn entry(&self, name: &str, instance: &str, sequence: u64, message: &str) -> HistoryEntry {
        let config = self.config();
        let rule = config.forward(name).unwrap();
        HistoryEntry::for_rule(
            instance.into(),
            EngineEvent {
                context: None,
                server_id: None,
                sequence,
                timestamp_ms: sequence,
                forward_id: Some(rule.id.clone()),
                message: message.into(),
            },
            rule,
            config.server(&rule.server_id).unwrap(),
        )
    }
    fn append(&self, entry: &HistoryEntry) {
        history::append_history(&self.journal(), entry).unwrap();
    }
    fn rotate(&self, entry: &HistoryEntry) {
        let length = fs::metadata(self.journal()).unwrap().len();
        let mut file = OpenOptions::new()
            .append(true)
            .open(self.journal())
            .unwrap();
        file.write_all(&vec![b'\n'; (MAX_LOG_BYTES - length) as usize])
            .unwrap();
        drop(file);
        self.append(entry);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let mut child = Stream::start(self.directory.path(), &["daemon", "stop"]);
        let _ = child.finish(std::time::Duration::from_secs(10));
    }
}

fn one_named(snapshot: &Value, name: &str) -> bool {
    snapshot["forwards"]
        .as_array()
        .is_some_and(|rules| rules.len() == 1 && rules[0]["name"] == name)
}

#[test]
fn status_watch_observes_offline_online_restart_rename_delete_and_ctrl_c() {
    let fixture = Fixture::new();
    fixture.add("web", "3000", "apps");
    let mut stream = fixture.stream(&["status", "--watch"]);
    stream.value_until(|value| value["daemon_running"] == false && one_named(value, "web"));
    fixture.run(&["daemon", "start"]);
    let first = stream.value_until(|value| value["daemon_running"] == true);
    fixture.run(&["edit", "web", "--rename", "renamed"]);
    stream.value_until(|value| one_named(value, "renamed"));
    fixture.run(&["daemon", "restart"]);
    stream.value_until(|value| {
        value["daemon_running"] == true
            && value["daemon_instance_id"] != first["daemon_instance_id"]
    });
    fixture.run(&["remove", "renamed"]);
    stream.value_until(|value| value["forwards"].as_array().is_some_and(Vec::is_empty));
    fixture.run(&["daemon", "stop"]);
    stream.value_until(|value| value["daemon_running"] == false);
    stream.interrupt();
    assert!(stream.errors.is_empty(), "{:?}", stream.errors);
}

#[test]
fn named_status_watch_tracks_stable_id_across_rename_then_shows_deletion() {
    let fixture = Fixture::new();
    fixture.add("web", "3000", "apps");
    let id = fixture.config().forward("web").unwrap().id.clone();
    let mut stream = fixture.stream(&["status", "web", "--watch"]);
    stream.value_until(|value| one_named(value, "web"));
    fixture.run(&["edit", "web", "--rename", "renamed"]);
    let renamed = stream.value_until(|value| one_named(value, "renamed"));
    assert_eq!(renamed["forwards"][0]["id"], id);
    fixture.run(&["remove", "renamed"]);
    stream.value_until(|value| value["forwards"].as_array().is_some_and(Vec::is_empty));
    stream.interrupt();
}

#[test]
fn group_status_watch_includes_new_members_and_survives_an_empty_group() {
    let fixture = Fixture::new();
    fixture.add("first", "3000", "apps");
    let mut stream = fixture.stream(&["status", "--group", "apps", "--watch"]);
    stream.value_until(|value| one_named(value, "first"));
    fixture.add("second", "3001", "apps");
    stream.value_until(|value| {
        value["forwards"]
            .as_array()
            .is_some_and(|rules| rules.len() == 2)
    });
    fixture.run(&["remove", "--group", "apps"]);
    stream.value_until(|value| value["forwards"].as_array().is_some_and(Vec::is_empty));
    fixture.add("third", "3002", "apps");
    stream.value_until(|value| one_named(value, "third"));
    stream.interrupt();
}

#[test]
fn server_status_watch_keeps_its_identity_after_server_rename_and_removal() {
    let fixture = Fixture::new();
    fixture.add("web", "3000", "apps");
    let mut stream = fixture.stream(&["status", "--server", "stream-fixture", "--watch"]);
    stream.value_until(|value| one_named(value, "web"));
    fixture.run(&[
        "server",
        "edit",
        "stream-fixture",
        "--rename",
        "renamed-server",
    ]);
    stream.value_until(|value| value["forwards"][0]["server"] == "renamed-server");
    fixture.run(&["remove", "web"]);
    fixture.run(&["server", "remove", "renamed-server"]);
    stream.value_until(|value| value["forwards"].as_array().is_some_and(Vec::is_empty));
    stream.interrupt();
}

#[test]
fn watch_rejects_initially_unknown_selectors_instead_of_waiting_forever() {
    let fixture = Fixture::new();
    fixture.add("web", "3000", "apps");
    for selector in [
        vec!["missing"],
        vec!["--group", "missing"],
        vec!["--server", "missing"],
    ] {
        let mut args = vec!["status", "--watch"];
        args.extend(selector);
        let mut stream = fixture.stream(&args);
        let status = stream.finish(std::time::Duration::from_secs(5));
        assert_eq!(status.code(), Some(2));
        assert!(stream.output.is_empty());
        assert_eq!(stream.errors.len(), 1);
        let error: Value = serde_json::from_str(&stream.errors[0]).unwrap();
        assert_eq!(error["error"]["code"], "not_found");
    }
}

#[test]
fn offline_watch_reports_malformed_draft_in_json_then_recovers_when_repaired() {
    let fixture = Fixture::new();
    fixture.add("web", "3000", "apps");
    let path = fixture.directory.path().join("config.toml");
    let good = fs::read(&path).unwrap();
    let mut stream = fixture.stream(&["status", "apps", "--watch"]);
    stream
        .value_until(|value| one_named(value, "web") && value["warnings"] == serde_json::json!([]));
    fs::write(&path, "[malformed").unwrap();
    let snapshot = stream.value_until(|value| {
        value["warnings"]
            .as_array()
            .is_some_and(|warnings| !warnings.is_empty())
    });
    assert!(one_named(&snapshot, "web"));
    assert_eq!(snapshot["daemon_running"], false);
    fs::write(path, good).unwrap();
    stream
        .value_until(|value| one_named(value, "web") && value["warnings"] == serde_json::json!([]));
    stream.interrupt();
    assert!(
        stream.errors.is_empty(),
        "JSON snapshot warnings must not be mixed into stderr"
    );
}

#[test]
fn logs_follow_old_name_survives_rename_deletion_restart_and_rotation_without_duplicates() {
    let fixture = Fixture::new();
    fixture.add("web", "3000", "apps");
    fixture.append(&fixture.entry("web", "manual", 1, "before-rename"));
    let mut stream = fixture.stream(&["logs", "web", "--follow", "--tail", "1"]);
    stream.value_until(|value| value["event"]["message"] == "before-rename");
    fixture.run(&["edit", "web", "--rename", "renamed"]);
    stream.value_until(|value| value["forward_name"] == "renamed");
    fixture.run(&["daemon", "restart"]);
    let mut marker = fixture.entry("renamed", "manual", 2, "before-delete");
    let id = marker.event.forward_id.clone().unwrap();
    fixture.run(&["remove", "renamed"]);
    stream.value_until(|value| {
        value["event"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("removed"))
    });
    fixture.run(&["daemon", "stop"]);
    // The daemon is stopped before direct history writes, preserving one writer.
    fixture.rotate(&marker);
    marker.event.sequence = 3;
    marker.event.message = "after-two-rotations".into();
    fixture.rotate(&marker);
    let retained = history::read_history(&fixture.journal()).unwrap();
    assert!(
        retained
            .entries
            .iter()
            .all(|entry| entry.forward_name.as_deref() != Some("web"))
    );
    stream.value_until(|value| value["event"]["message"] == "after-two-rotations");
    marker.daemon_instance_id = "new-instance".into();
    marker.event.sequence = 1;
    marker.event.message = "sequence-reset".into();
    fixture.append(&marker);
    stream.value_until(|value| value["event"]["message"] == "sequence-reset");
    stream.interrupt();
    let entries: Vec<HistoryEntry> = stream
        .output
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let keys: HashSet<_> = entries
        .iter()
        .map(|entry| (&entry.daemon_instance_id, entry.event.sequence))
        .collect();
    assert_eq!(keys.len(), entries.len(), "follow replayed an event");
    assert!(
        entries
            .iter()
            .all(|entry| entry.event.forward_id.as_deref() == Some(&id))
    );
}

#[test]
fn logs_follow_tail_zero_skips_old_events_deduplicates_warnings_and_exits_on_ctrl_c() {
    let fixture = Fixture::new();
    fixture.add("web", "3000", "apps");
    let mut marker = fixture.entry("web", "manual", 1, "old-entry");
    fs::write(fixture.journal(), b"malformed-history\n").unwrap();
    fixture.append(&marker);
    let mut stream = fixture.stream(&["logs", "--follow", "--tail", "0"]);
    stream.warning_until("skipped 1 incomplete");
    for (sequence, message) in [(2, "first-fresh"), (3, "second-fresh")] {
        marker.event.sequence = sequence;
        marker.event.message = message.into();
        fixture.append(&marker);
        stream.value_until(|value| value["event"]["message"] == message);
    }
    marker.event.sequence = 9;
    marker.event.message = "gap".into();
    fixture.append(&marker);
    stream.warning_until("expired during log rotation");
    stream.value_until(|value| value["event"]["message"] == "gap");
    stream.interrupt();
    assert_eq!(
        stream
            .errors
            .iter()
            .filter(|line| line.contains("skipped 1 incomplete"))
            .count(),
        1
    );
    assert_eq!(
        stream
            .errors
            .iter()
            .filter(|line| line.contains("expired during log rotation"))
            .count(),
        1
    );
    assert_eq!(stream.output.len(), 3);
    assert!(stream.output.iter().all(|line| !line.contains("old-entry")));
}
