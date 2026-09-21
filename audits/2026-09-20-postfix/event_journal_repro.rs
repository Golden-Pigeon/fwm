// Compile with run_event_journal_repro.py. Imports the unchanged production file.
#[path = "../../crates/fwm/src/daemon/events.rs"]
mod journal;

fn main() {
    use fwm_core::{history::{HistoryFilter, read_history}, model::{Config, EngineEvent}};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    let mut config: Config = serde_json::from_value(serde_json::json!({
        "schema_version":3,
        "servers":[
            {"id":"server-a","name":"alpha","host":"127.0.0.1"},
            {"id":"server-b","name":"beta","host":"127.0.0.2"}
        ],
        "forwards":[{"id":"rule","name":"old-rule","group":"old-group","server_id":"server-a",
            "kind":"dynamic","listen":"127.0.0.1:1080","desired_state":"stopped"}]
    })).unwrap();
    config.validate().unwrap();
    let mut journal = journal::EventJournal::new("audit".into(), path.clone());
    journal.set_config(&config);
    // The engine produced this while the old server/group/name was current.
    // Its broadcast can be pending while an RPC owns the application mutex.
    let pending = EngineEvent {
        sequence: 9, timestamp_ms: 1234, forward_id: Some("rule".into()),
        server_id: Some("server-a".into()), message: "old transport failed".into(),
    };
    config.forwards[0].name = "new-rule".into();
    config.forwards[0].group = Some("new-group".into());
    config.forwards[0].server_id = "server-b".into();
    journal.set_config(&config);
    journal.record_engine(pending);
    let history = read_history(&path).unwrap();
    let old = HistoryFilter { server: Some("server-a".into()), ..Default::default() }.select(&history.entries, Some(&config));
    let new = HistoryFilter { server: Some("server-b".into()), ..Default::default() }.select(&history.entries, Some(&config));
    println!("{}", serde_json::json!({
        "entry": history.entries[0], "old_server_results":old.len(), "new_server_results":new.len(),
        "original_timestamp_ms":1234
    }));
}
