// Functional audit only: unchanged production modules, memory and temporary files.
mod model {
    pub use fwm_core::model::*;
}
#[path = "../../crates/fwm/src/daemon/events.rs"]
mod journal;
#[path = "../../crates/fwm-core/src/engine/state.rs"]
mod runtime;

use fwm_core::{history::read_history, model::*};
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

fn config() -> Config {
    let value: Config = serde_json::from_value(json!({
        "schema_version":3,
        "servers":[{"id":"a","name":"alpha","host":"127.0.0.1"},
                   {"id":"b","name":"beta","host":"127.0.0.2"}],
        "forwards":[{"id":"rule","name":"old-rule","group":"old-group","server_id":"a",
            "kind":"dynamic","listen":"127.0.0.1:1080","desired_state":"stopped"}]
    }))
    .unwrap();
    value.validate().unwrap();
    value
}

fn event(server_id: Option<&str>) -> EngineEvent {
    EngineEvent {
        sequence: 42,
        timestamp_ms: 1234,
        forward_id: Some("rule".into()),
        server_id: server_id.map(str::to_owned),
        message: "old operation completed".into(),
    }
}

fn main() {
    let mut results = vec![];
    for case in [
        "move_unscoped",
        "move_explicit_server",
        "rename_group",
        "delete",
        "recreate_new_id",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let mut journal = journal::EventJournal::new("functional-audit".into(), path.clone());
        let mut config = config();
        journal.set_config(&config);
        let pending = event((case == "move_explicit_server").then_some("a"));
        match case {
            "move_unscoped" | "move_explicit_server" => {
                config.forwards[0].server_id = "b".into();
                config.forwards[0].name = "new-rule".into();
                config.forwards[0].group = Some("new-group".into());
            }
            "rename_group" => config.forwards[0].group = Some("new-group".into()),
            "delete" => config.forwards.clear(),
            "recreate_new_id" => {
                config.forwards[0].id = "replacement".into();
                config.forwards[0].server_id = "b".into();
                config.forwards[0].group = Some("new-group".into());
            }
            _ => unreachable!(),
        }
        config.validate().unwrap();
        journal.set_config(&config);
        journal.record_engine(pending);
        let history = read_history(&path).unwrap();
        assert!(history.warnings.is_empty());
        assert_eq!(history.entries.len(), 1);
        let entry = &history.entries[0];
        let reply = journal.since(0);
        assert_eq!(reply.events.len(), 1);
        assert_eq!(reply.next_sequence, 1);
        assert!(!reply.has_more);
        assert_ne!(entry.event.timestamp_ms, 1234);
        let defect = match case {
            "move_unscoped" | "move_explicit_server" => {
                assert_eq!(entry.server_id.as_deref(), Some("b"));
                assert_eq!(entry.group.as_deref(), Some("new-group"));
                if case == "move_explicit_server" {
                    assert_eq!(entry.event.server_id.as_deref(), Some("a"));
                } else {
                    assert_eq!(entry.event.server_id.as_deref(), Some("b"));
                }
                true
            }
            "rename_group" => {
                assert_eq!(entry.group.as_deref(), Some("new-group"));
                true
            }
            _ => {
                assert_eq!(entry.server_id.as_deref(), Some("a"));
                assert_eq!(entry.group.as_deref(), Some("old-group"));
                assert_eq!(entry.forward_name.as_deref(), Some("old-rule"));
                false
            }
        };
        results.push(json!({"case":case,"incorrect_scope":defect,"source_timestamp_preserved":false,"entry":entry}));
    }

    let config = config();
    let spec = config.forwards[0].clone();
    let statuses = Arc::new(Mutex::new(HashMap::from([(
        "rule".into(),
        runtime::StatusEntry {
            generation: 1,
            connection_key: "a".into(),
            spec: spec.clone(),
            status: ForwardStatus {
                id: "rule".into(),
                name: "old-rule".into(),
                group: Some("old-group".into()),
                server: "alpha".into(),
                kind: "dynamic".into(),
                listen: "127.0.0.1:1080".into(),
                target: None,
                desired_state: DesiredState::Stopped,
                state: RuntimeState::Stopped,
                retry_count: 0,
                next_retry_unix_ms: None,
                last_error: None,
                active_connections: 0,
            },
        },
    )])));
    let events = runtime::Events::new();
    let mut receiver = events.subscribe();
    let old_rule = runtime::Rule {
        spec,
        generation: 1,
        statuses: statuses.clone(),
        events,
    };
    statuses.lock().unwrap().get_mut("rule").unwrap().generation = 2;
    old_rule.update(RuntimeState::Backoff, Some("old state".into()), 1, None);
    assert!(receiver.try_recv().is_err());
    assert_eq!(
        statuses.lock().unwrap()["rule"].status.state,
        RuntimeState::Stopped
    );
    old_rule.connection_error("old generation completed late");
    let stale = receiver.try_recv().unwrap();
    assert!(stale.server_id.is_none());
    assert_eq!(stale.forward_id.as_deref(), Some("rule"));
    results.push(json!({"case":"stale_generation","state_update_suppressed":true,"connection_error_emitted":stale}));
    statuses.lock().unwrap().clear();
    old_rule.connection_error("deleted operation completed late");
    let deleted = receiver.try_recv().unwrap();
    results.push(json!({"case":"deleted_generation","connection_error_emitted":deleted}));
    println!("{}", serde_json::to_string_pretty(&json!({"cases":results,"assertions_passed":true,
        "scope":"unchanged EventJournal and Rule modules; no sockets, daemon, SSH, agent or system services"})).unwrap());
}
