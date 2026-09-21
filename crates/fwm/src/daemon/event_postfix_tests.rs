use super::*;

#[test]
fn queued_engine_events_keep_producer_context_and_time_after_edits() {
    for change in [
        "rename",
        "move",
        "group",
        "delete",
        "replace",
        "server-rename",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("events.jsonl");
        let mut server = ServerProfile::new("old-server");
        server.id = "server-a".into();
        let mut config = Config {
            servers: vec![server],
            ..Default::default()
        };
        let captured = EngineEvent {
            context: Some(EventContext {
                forward_name: Some("old-rule".into()),
                server_id: Some("server-a".into()),
                server_name: Some("old-server".into()),
                group: Some("old-group".into()),
            }),
            server_id: Some("server-a".into()),
            forward_id: Some("rule".into()),
            sequence: 99,
            timestamp_ms: 1234,
            message: change.into(),
        };
        let mut journal = EventJournal::new("test".into(), path.clone());
        journal.set_config(&config);
        journal.remember(
            "rule".into(),
            HistoryLabels {
                forward_name: Some("new-rule".into()),
                server_id: Some("server-b".into()),
                server_name: Some("new-server".into()),
                group: Some("new-group".into()),
            },
        );
        config.servers[0].name = "new-server".into();
        if change == "delete" {
            config.servers.clear();
        }
        journal.set_config(&config);
        journal.record_engine(captured);
        let read = read_history(&path).unwrap();
        let entry = &read.entries[0];
        assert_eq!(entry.event.timestamp_ms, 1234);
        assert_eq!(entry.event.sequence, 1);
        assert_eq!(entry.forward_name.as_deref(), Some("old-rule"));
        assert_eq!(entry.server_name.as_deref(), Some("old-server"));
        assert_eq!(entry.group.as_deref(), Some("old-group"));
        assert_eq!(entry.server_id.as_deref(), Some("server-a"));
        assert_eq!(entry.event.server_id, entry.server_id);
        assert_eq!(journal.since(0).events[0].timestamp_ms, 1234);
    }
}

#[test]
fn oversized_event_requests_resync_without_silently_advancing_cursor() {
    let temp = tempfile::tempdir().unwrap();
    let mut journal = EventJournal::new("test".into(), temp.path().join("events.jsonl"));
    journal.sequence = 1;
    journal.events.push_back(EngineEvent {
        context: None,
        sequence: 1,
        timestamp_ms: 1,
        server_id: None,
        forward_id: Some("x".repeat(256 * 1024)),
        message: "message".into(),
    });
    let reply = journal.since(0);
    assert!(reply.events.is_empty());
    assert!(reply.resync_required);
    assert!(reply.has_more);
    assert_eq!(reply.next_sequence, 0);
}

#[test]
fn contextless_queued_events_do_not_acquire_the_current_rules_scope_when_read() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("events.jsonl");
    let mut journal = EventJournal::new("test".into(), path.clone());
    journal.remember(
        "rule".into(),
        HistoryLabels {
            forward_name: Some("new-name".into()),
            server_id: Some("new-server".into()),
            server_name: Some("new-server-name".into()),
            group: Some("new-group".into()),
        },
    );
    journal.record_engine(EngineEvent {
        context: None,
        server_id: Some("old-server".into()),
        forward_id: Some("rule".into()),
        sequence: 42,
        timestamp_ms: 123,
        message: "legacy queued event".into(),
    });
    journal.record(Some("rule".into()), "new event".into());
    let mut read = read_history(&path).unwrap();
    fwm_core::history::enrich_legacy(&mut read.entries, None);
    let old = &read.entries[0];
    assert_eq!(old.server_id.as_deref(), Some("old-server"));
    assert_eq!(old.event.server_id, old.server_id);
    assert!(old.forward_name.is_none());
    assert!(old.server_name.is_none());
    assert!(old.group.is_none());
    assert_eq!(old.event.timestamp_ms, 123);
    assert_eq!(old.label(), "rule");
}
