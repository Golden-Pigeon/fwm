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
                server: Some(server.clone())
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
