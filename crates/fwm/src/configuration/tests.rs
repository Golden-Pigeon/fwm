use super::*;
use fwm_core::model::{ConnectionMode, ForwardSpec, RemoteCleanup, ServerProfile, Tunnel};

fn fixture() -> (Config, ForwardSpec) {
    let mut server = ServerProfile::new("dev");
    server.host = Some("127.0.0.1".into());
    let rule = ForwardSpec {
        id: "rule".into(),
        name: "web".into(),
        group: None,
        server_id: server.id.clone(),
        tunnel: Tunnel::Local {
            listen: "127.0.0.1:3000".parse().unwrap(),
            target: "internal:8080".parse().unwrap(),
        },
        desired_state: DesiredState::Stopped,
        connection_mode: ConnectionMode::Shared,
        remote_cleanup: RemoteCleanup::Off,
    };
    (
        Config {
            servers: vec![server],
            ..Config::default()
        },
        rule,
    )
}

#[test]
fn legacy_put_and_single_replace_preserve_identity_without_changing_input() {
    let (config, rule) = fixture();
    let created = prepare(
        &config,
        &Command::PutForward {
            forward: rule.clone(),
        },
    )
    .unwrap();
    assert!(config.forwards.is_empty());
    let mut edited = rule.clone();
    edited.name = "renamed".into();
    for command in [
        Command::PutForward {
            forward: edited.clone(),
        },
        Command::PutForwardWithServer {
            forward: edited.clone(),
            server: None,
            target_server: None,
        },
    ] {
        let changed = prepare(&created.config, &command).unwrap();
        assert_eq!(changed.config.forwards, vec![edited.clone()]);
        assert!(changed.selected.is_none());
        assert_eq!(created.config.forwards, vec![rule.clone()]);
    }
}

#[test]
fn legacy_and_batch_delete_have_the_same_control_intent() {
    let (mut config, rule) = fixture();
    config.forwards.push(rule.clone());
    for command in [
        Command::RemoveForward {
            selector: "web".into(),
        },
        Command::RemoveForwards {
            selection: Selection::Forward("rule".into()),
        },
    ] {
        let changed = prepare(&config, &command).unwrap();
        assert!(changed.config.forwards.is_empty());
        assert_eq!(changed.selected, Some(vec![rule.id.clone()]));
        assert_eq!(config.forwards, vec![rule.clone()]);
    }
}

#[test]
fn single_replace_with_new_server_is_atomic_and_checks_references() {
    let (mut config, rule) = fixture();
    config.forwards.push(rule.clone());
    let mut server = ServerProfile::new("new");
    server.host = Some("127.0.0.2".into());
    assert!(
        prepare(
            &config,
            &Command::PutForwardWithServer {
                forward: rule.clone(),
                server: Some(server.clone()),
                target_server: None,
            }
        )
        .is_err()
    );
    let mut moved = rule.clone();
    moved.server_id = server.id.clone();
    let changed = prepare(
        &config,
        &Command::PutForwardWithServer {
            forward: moved.clone(),
            server: Some(server),
            target_server: None,
        },
    )
    .unwrap();
    assert_eq!(changed.config.forwards, vec![moved]);
    assert_eq!(changed.config.servers.len(), 2);
    assert_eq!(config.servers.len(), 1);
    assert_eq!(config.forwards, vec![rule]);
}

#[test]
fn failed_plans_and_conflicting_activation_never_change_input() {
    let (mut config, rule) = fixture();
    config.forwards.push(rule.clone());
    let mut other = rule.clone();
    other.id = "other".into();
    other.name = "other".into();
    config.forwards.push(other);
    let original = config.clone();
    for command in [
        Command::SetDesired {
            selection: Selection::All,
            state: DesiredState::Running,
        },
        Command::RemoveForward {
            selector: "missing".into(),
        },
        Command::RemoveServer {
            selector: "dev".into(),
        },
        Command::PutServer {
            server: ServerProfile::new("invalid"),
        },
        Command::Status,
    ] {
        assert!(prepare(&config, &command).is_err());
        assert_eq!(config, original);
    }
    let change = prepare(
        &config,
        &Command::SetDesired {
            selection: Selection::Forward(rule.id.clone()),
            state: DesiredState::Running,
        },
    )
    .unwrap();
    assert_eq!(change.selected, Some(vec![rule.id]));
    assert_eq!(
        change.config.forwards[0].desired_state,
        DesiredState::Running
    );
    assert_eq!(
        change.config.forwards[1].desired_state,
        DesiredState::Stopped
    );
}

#[test]
fn offline_restart_enables_only_selected_rules_without_losing_command_semantics() {
    let (mut config, rule) = fixture();
    config.forwards.push(rule.clone());
    let mut other = rule.clone();
    other.id = "other".into();
    other.name = "other".into();
    config.forwards.push(other);
    let changed = prepare(
        &config,
        &Command::Restart {
            selection: Selection::Forward("web".into()),
        },
    )
    .unwrap();
    assert_eq!(
        changed.config.forwards[0].desired_state,
        DesiredState::Running
    );
    assert_eq!(
        changed.config.forwards[1].desired_state,
        DesiredState::Stopped
    );
    assert_eq!(changed.selected, Some(vec![rule.id]));
    assert!(changed.message.starts_with("Restart requested"));
}

fn relay_parts() -> (ServerProfile, ServerProfile, ForwardSpec) {
    let mut source = ServerProfile::new("a");
    source.host = Some("127.0.0.1".into());
    let mut target = ServerProfile::new("c");
    target.host = Some("127.0.0.2".into());
    let relay = ForwardSpec {
        id: "relay-rule".into(),
        name: "a-to-c".into(),
        group: None,
        server_id: source.id.clone(),
        tunnel: Tunnel::Relay {
            listen: "127.0.0.1:8080".parse().unwrap(),
            target: "localhost:9000".parse().unwrap(),
            target_server_id: target.id.clone(),
        },
        desired_state: DesiredState::Running,
        connection_mode: ConnectionMode::Dedicated,
        remote_cleanup: RemoteCleanup::Verified,
    };
    (source, target, relay)
}

#[test]
fn relay_create_commits_both_aliases_and_the_rule_in_one_transaction() {
    let (source, target, relay) = relay_parts();
    let mut forwarded = relay.clone();
    forwarded.server_id = source.id.clone();
    let changed = prepare(
        &Config::default(),
        &Command::CreateForwards {
            forwards: vec![forwarded],
            server: Some(source),
            target_server: Some(target),
        },
    )
    .unwrap()
    .config;
    assert_eq!(changed.servers.len(), 2);
    assert_eq!(changed.forwards.len(), 1);
    changed.validate().unwrap();
    // A dangling target reference fails the whole batch before any commit.
    let (source, target, relay) = relay_parts();
    let mut orphan = relay.clone();
    orphan.tunnel = Tunnel::Relay {
        listen: relay.tunnel.listen(),
        target: relay.tunnel.target().unwrap().clone(),
        target_server_id: "missing".into(),
    };
    assert!(
        prepare(
            &Config::default(),
            &Command::CreateForwards {
                forwards: vec![orphan],
                server: Some(source),
                target_server: Some(target),
            }
        )
        .is_err()
    );
}

#[test]
fn server_removal_is_blocked_by_either_relay_endpoint() {
    let (source, target, relay) = relay_parts();
    let config = Config {
        servers: vec![source.clone(), target.clone()],
        forwards: vec![relay],
        ..Config::default()
    };
    for selector in ["a", "c"] {
        let error = prepare(
            &config,
            &Command::RemoveServer {
                selector: selector.into(),
            },
        )
        .err()
        .map(|error| error.code);
        assert_eq!(error.as_deref(), Some("server_in_use"), "{selector}");
    }
    let removed = prepare(
        &Config {
            servers: vec![source, target],
            forwards: vec![],
            ..Config::default()
        },
        &Command::RemoveServer {
            selector: "a".into(),
        },
    )
    .unwrap();
    assert_eq!(removed.config.servers.len(), 1);
    assert_eq!(removed.config.servers[0].name, "c");
}
