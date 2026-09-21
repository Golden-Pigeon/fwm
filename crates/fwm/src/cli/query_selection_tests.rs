use super::*;
use fwm_core::model::{
    ConnectionMode, DesiredState, ForwardSpec, RemoteCleanup, ServerProfile, Tunnel,
};

fn fixture() -> (Config, Vec<HistoryEntry>) {
    let mut server = ServerProfile::new("dev");
    server.id = "server-id".into();
    server.host = Some("127.0.0.1".into());
    let rule = ForwardSpec {
        id: "original-id".into(),
        name: "web".into(),
        group: Some("batch".into()),
        server_id: server.id.clone(),
        tunnel: Tunnel::Dynamic {
            listen: "127.0.0.1:3000".parse().unwrap(),
        },
        desired_state: DesiredState::Stopped,
        connection_mode: ConnectionMode::Shared,
        remote_cleanup: RemoteCleanup::Off,
    };
    let config = Config {
        servers: vec![server],
        forwards: vec![rule],
        ..Default::default()
    };
    let entry = entry(&config, 1);
    (config, vec![entry])
}
fn entry(config: &Config, n: u64) -> HistoryEntry {
    let rule = &config.forwards[0];
    HistoryEntry::for_rule(
        "test".into(),
        fwm_core::model::EngineEvent {
            context: None,
            sequence: n,
            timestamp_ms: 0,
            forward_id: Some(rule.id.clone()),
            server_id: None,
            message: "event".into(),
        },
        rule,
        &config.servers[0],
    )
}

#[test]
fn server_follow_pins_identity_through_rename_deletion_and_label_expiry() {
    let (mut config, entries) = fixture();
    let mut filter = FollowFilter::new(HistoryFilter {
        server: Some("dev".into()),
        ..Default::default()
    });
    assert_eq!(filter.select(&entries, Some(&config)).len(), 1);
    config.servers[0].name = "prod".into();
    let renamed = entry(&config, 2);
    assert_eq!(
        filter
            .select(std::slice::from_ref(&renamed), Some(&config))
            .len(),
        1
    );
    config.forwards.clear();
    config.servers.clear();
    assert_eq!(
        filter
            .select(std::slice::from_ref(&renamed), Some(&config))
            .len(),
        1
    );
    let mut unrelated = renamed;
    unrelated.server_name = Some("dev".into());
    unrelated.server_id = Some("new-server".into());
    assert!(filter.select(&[unrelated], Some(&config)).is_empty());
}

#[test]
fn rule_follow_does_not_jump_to_a_new_rule_reusing_the_original_name() {
    let (mut config, entries) = fixture();
    let mut filter = FollowFilter::new(HistoryFilter {
        name: Some("web".into()),
        ..Default::default()
    });
    assert_eq!(filter.select(&entries, Some(&config)).len(), 1);
    config.forwards[0].name = "renamed".into();
    let renamed = entry(&config, 2);
    let mut other = config.forwards[0].clone();
    other.name = "web".into();
    other.id = "replacement-id".into();
    config.forwards.insert(0, other);
    let replacement = entry(&config, 3);
    let selected = filter.select(&[renamed, replacement], Some(&config));
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].event.forward_id.as_deref(), Some("original-id"));
}

#[test]
fn explicit_group_and_shorthand_follow_keep_event_time_group_membership() {
    let (mut config, entries) = fixture();
    let mut short = FollowFilter::new(HistoryFilter {
        name: Some("batch".into()),
        ..Default::default()
    });
    let mut explicit = FollowFilter::new(HistoryFilter {
        group: Some("batch".into()),
        ..Default::default()
    });
    assert_eq!(short.select(&entries, Some(&config)).len(), 1);
    assert_eq!(explicit.select(&entries, Some(&config)).len(), 1);
    config.forwards[0].group = Some("other".into());
    let moved = entry(&config, 2);
    assert!(
        short
            .select(std::slice::from_ref(&moved), Some(&config))
            .is_empty()
    );
    assert!(explicit.select(&[moved], Some(&config)).is_empty());
}

#[test]
fn historical_id_wins_over_another_deleted_rules_name_and_remains_pinned() {
    let (mut config, mut entries) = fixture();
    config.forwards[0].id = "replacement-id".into();
    config.forwards[0].name = "original-id".into();
    entries.push(entry(&config, 2));
    config.forwards.clear();
    let base = HistoryFilter {
        name: Some("original-id".into()),
        ..Default::default()
    };
    assert_eq!(base.select(&entries, Some(&config)).len(), 1);
    let mut follow = FollowFilter::new(base);
    let selected = follow.select(&entries, Some(&config));
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].event.forward_id.as_deref(), Some("original-id"));
    assert!(follow.select(&entries[1..], Some(&config)).is_empty());
}
