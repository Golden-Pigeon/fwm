use super::*;
use crate::model::{ConnectionMode, DesiredState, RemoteCleanup, Tunnel};

fn setup() -> (Config, Vec<HistoryEntry>) {
    let mut server = ServerProfile::new("dev");
    server.id = "server-id".into();
    server.host = Some("127.0.0.1".into());
    let old = ForwardSpec {
        id: "old-id".into(),
        name: "task".into(),
        group: Some("old-group".into()),
        server_id: server.id.clone(),
        tunnel: Tunnel::Dynamic {
            listen: "127.0.0.1:3000".parse().unwrap(),
        },
        desired_state: DesiredState::Stopped,
        connection_mode: ConnectionMode::Shared,
        remote_cleanup: RemoteCleanup::Off,
    };
    let mut new = old.clone();
    new.id = "new-id".into();
    new.name = "new".into();
    new.group = Some("task".into());
    new.tunnel = Tunnel::Dynamic {
        listen: "127.0.0.1:3001".parse().unwrap(),
    };
    let entries = vec![
        HistoryEntry::for_rule("one".into(), event(1), &old, &server),
        HistoryEntry::for_rule("one".into(), event(2), &new, &server),
    ];
    let mut renamed = old;
    renamed.name = "archived".into();
    (
        Config {
            servers: vec![server],
            forwards: vec![renamed, new],
            ..Default::default()
        },
        entries,
    )
}
fn event(sequence: u64) -> EngineEvent {
    EngineEvent {
        context: None,
        sequence,
        timestamp_ms: 0,
        forward_id: Some(if sequence == 1 { "old-id" } else { "new-id" }.into()),
        server_id: None,
        message: "sample".into(),
    }
}

#[test]
fn current_group_takes_priority_over_historical_rule_alias_and_matches_explicit_group() {
    let (config, entries) = setup();
    let short = HistoryFilter {
        name: Some("task".into()),
        ..Default::default()
    }
    .select(&entries, Some(&config));
    let explicit = HistoryFilter {
        group: Some("task".into()),
        ..Default::default()
    }
    .select(&entries, Some(&config));
    assert_eq!(short.len(), 1);
    assert_eq!(short[0].event.forward_id.as_deref(), Some("new-id"));
    assert_eq!(
        serde_json::to_value(short).unwrap(),
        serde_json::to_value(explicit).unwrap()
    );
    let old = HistoryFilter {
        name: Some("old-id".into()),
        ..Default::default()
    }
    .select(&entries, Some(&config));
    assert_eq!(old.len(), 1);
}

#[test]
fn current_rule_name_does_not_mix_with_another_rules_historical_alias() {
    let (mut config, entries) = setup();
    config.forwards[1].group = None;
    config.forwards[1].name = "task".into();
    let found = HistoryFilter {
        name: Some("task".into()),
        ..Default::default()
    }
    .select(&entries, Some(&config));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].event.forward_id.as_deref(), Some("new-id"));
}

#[test]
fn renamed_group_shorthand_uses_event_time_membership_and_includes_no_old_group_events() {
    let (mut config, mut entries) = setup();
    config.forwards[0].group = Some("renamed".into());
    entries.push(HistoryEntry::for_rule(
        "two".into(),
        event(1),
        &config.forwards[0],
        &config.servers[0],
    ));
    let short = HistoryFilter {
        name: Some("renamed".into()),
        ..Default::default()
    }
    .select(&entries, Some(&config));
    assert_eq!(short.len(), 1);
    assert_eq!(short[0].group.as_deref(), Some("renamed"));
}
