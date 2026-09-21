use std::fs;

use super::*;

fn event(instance: &str, sequence: u64, name: &str) -> HistoryEntry {
    HistoryEntry {
        daemon_instance_id: instance.into(),
        event: EngineEvent {
            context: None,
            server_id: None,
            sequence,
            timestamp_ms: 0,
            forward_id: Some("rule-id".into()),
            message: format!("event {sequence}"),
        },
        forward_name: Some(name.into()),
        server_id: Some("server-id".into()),
        server_name: Some("dev".into()),
        group: Some("web".into()),
    }
}

#[test]
fn offline_history_keeps_deleted_and_renamed_rule_labels_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    append_history(&path, &event("before-restart", 1, "old-name")).unwrap();
    append_history(&path, &event("after-restart", 1, "new-name")).unwrap();
    let read = read_history(&path).unwrap();
    assert!(read.warnings.is_empty());
    let old = HistoryFilter {
        name: Some("old-name".into()),
        ..Default::default()
    }
    .select(&read.entries, Some(&Config::default()));
    assert_eq!(old.len(), 2);
    assert_eq!(old[0].forward_name.as_deref(), Some("old-name"));
    assert_eq!(old[1].forward_name.as_deref(), Some("new-name"));
    assert_eq!(
        HistoryFilter {
            group: Some("web".into()),
            ..Default::default()
        }
        .select(&read.entries, None)
        .len(),
        2
    );
    assert_eq!(
        HistoryFilter {
            server: Some("dev".into()),
            ..Default::default()
        }
        .select(&read.entries, None)
        .len(),
        2
    );
    assert_eq!(
        HistoryFilter {
            name: Some("web".into()),
            ..Default::default()
        }
        .select(&read.entries, None)
        .len(),
        2
    );
    assert_eq!(tail(old, 1)[0].event.sequence, 1);
}

#[test]
fn rotation_bounds_both_files_and_read_order_and_tail_are_stable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    for sequence in 1..=1200 {
        let mut entry = event("one", sequence, "web");
        entry.event.message = "x".repeat(MAX_MESSAGE_BYTES);
        append_history(&path, &entry).unwrap();
    }
    assert!(fs::metadata(&path).unwrap().len() <= MAX_LOG_BYTES);
    assert!(fs::metadata(rotated_path(&path)).unwrap().len() <= MAX_LOG_BYTES);
    let read = read_history(&path).unwrap();
    assert!(read.warnings.is_empty());
    assert!(read.entries[0].event.sequence > 1);
    assert_eq!(read.entries.last().unwrap().event.sequence, 1200);
    assert!(
        read.entries
            .windows(2)
            .all(|pair| pair[0].event.sequence + 1 == pair[1].event.sequence)
    );
    let latest = tail(read.entries, 3);
    assert_eq!(
        latest
            .iter()
            .map(|entry| entry.event.sequence)
            .collect::<Vec<_>>(),
        vec![1198, 1199, 1200]
    );
}

#[test]
fn malformed_and_truncated_lines_are_reported_and_old_envelopes_still_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    let entry = event("old", 1, "web");
    let old = serde_json::json!({"daemon_instance_id":"old","event":entry.event});
    fs::write(&path, format!("{old}\nnot json\n{{\"partial\":")).unwrap();
    let read = read_history(&path).unwrap();
    assert_eq!(read.entries.len(), 1);
    assert!(read.entries[0].forward_name.is_none());
    assert_eq!(read.warnings.len(), 1);
    assert!(read.warnings[0].contains("skipped 2"));
}

#[test]
fn follow_deduplicates_rotation_and_accepts_reset_sequence_from_a_new_instance() {
    let initial = vec![event("first", 1, "web"), event("first", 2, "web")];
    let mut cursor = HistoryCursor::from_snapshot(&initial);
    assert!(cursor.take_new(&initial).0.is_empty());
    let mut rotated = initial.clone();
    rotated.push(event("first", 3, "web"));
    rotated.push(event("second", 1, "web"));
    let (fresh, gap) = cursor.take_new(&rotated);
    assert_eq!(fresh.len(), 2);
    assert!(!gap);
    assert!(cursor.take_new(&rotated).0.is_empty());
    let (fresh, gap) = cursor.take_new(&[event("second", 9, "web")]);
    assert_eq!(fresh.len(), 1);
    assert!(gap);
}

#[test]
fn utc_dates_are_human_readable() {
    assert_eq!(format_timestamp(0), "1970-01-01 00:00:00.000 UTC");
    assert_eq!(format_timestamp(1234), "1970-01-01 00:00:01.234 UTC");
}

#[test]
fn append_recovers_from_a_crashed_partial_write_and_bounds_json_escaping() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    fs::write(&path, b"{\"partial\":").unwrap();
    let mut entry = event("new", 1, "web");
    entry.event.message = "\0".repeat(MAX_MESSAGE_BYTES);
    append_history(&path, &entry).unwrap();
    let result = read_history(&path).unwrap();
    assert_eq!(result.entries.len(), 1);
    assert!(result.entries[0].event.message.ends_with("[truncated]"));
    assert_eq!(result.warnings.len(), 1);
    assert!(result.warnings[0].contains("skipped 1"));
}

#[test]
fn legacy_unknown_labels_are_not_guessed_from_a_later_event_or_current_config() {
    use crate::model::{ConnectionMode, DesiredState, RemoteCleanup, Tunnel};
    let mut server = ServerProfile::new("new-server");
    server.id = "new-server-id".into();
    let config = Config {
        forwards: vec![ForwardSpec {
            id: "rule-id".into(),
            name: "new-name".into(),
            group: Some("new-group".into()),
            server_id: server.id.clone(),
            tunnel: Tunnel::Dynamic {
                listen: "127.0.0.1:1080".parse().unwrap(),
            },
            desired_state: DesiredState::Stopped,
            connection_mode: ConnectionMode::Shared,
            remote_cleanup: RemoteCleanup::Off,
        }],
        servers: vec![server],
        ..Default::default()
    };
    let mut old = event("old", 1, "old-name");
    old.forward_name = None;
    old.event.server_id = Some("old-server-id".into());
    old.server_id = Some("old-server-id".into());
    old.server_name = Some("old-server".into());
    old.group = Some("old-group".into());
    let unlabeled = HistoryEntry::new("old".into(), old.event.clone());
    let later = HistoryEntry::for_rule(
        "new".into(),
        old.event.clone(),
        &config.forwards[0],
        &config.servers[0],
    );
    let mut entries = [old, unlabeled, later];
    enrich_legacy(&mut entries, Some(&config));
    assert!(entries[0].forward_name.is_none());
    assert_eq!(entries[0].server_id.as_deref(), Some("old-server-id"));
    assert_eq!(entries[0].server_name.as_deref(), Some("old-server"));
    assert_eq!(entries[0].group.as_deref(), Some("old-group"));
    assert!(entries[1].forward_name.is_none());
    assert_eq!(entries[1].server_id.as_deref(), Some("old-server-id"));
    assert!(entries[1].server_name.is_none());
    assert!(entries[1].group.is_none());
    assert_eq!(entries[1].label(), "rule-id");
}

#[test]
fn captured_context_remains_authoritative_in_constructors_and_legacy_projection() {
    let mut original = event("old", 1, "outer-wrong-name");
    original.event.context = Some(EventContext {
        forward_name: None,
        server_id: Some("captured-server".into()),
        server_name: Some("captured-name".into()),
        group: None,
    });
    original.event.server_id = Some("wrong-server".into());
    let projected = HistoryEntry::new("old".into(), original.event.clone());
    assert_eq!(projected.server_id.as_deref(), Some("captured-server"));
    assert_eq!(projected.event.server_id, projected.server_id);
    let server = ServerProfile::new("current-server");
    let explicitly_labeled =
        HistoryEntry::for_server("old".into(), original.event.clone(), &server);
    assert_eq!(
        explicitly_labeled.server_id.as_deref(),
        Some("captured-server")
    );
    let mut entries = [original];
    enrich_legacy(&mut entries, None);
    assert!(entries[0].forward_name.is_none());
    assert!(entries[0].group.is_none());
    assert_eq!(entries[0].server_name.as_deref(), Some("captured-name"));
    assert_eq!(entries[0].server_id, entries[0].event.server_id);
}
