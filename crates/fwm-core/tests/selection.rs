use fwm_core::{
    model::{
        Config, ConnectionMode, DesiredState, ForwardSpec, RemoteCleanup, ServerProfile, Tunnel,
    },
    selection::Selection,
};

fn fixture() -> Config {
    let mut dev = ServerProfile::new("dev");
    dev.host = Some("127.0.0.1".into());
    let mut other = ServerProfile::new("web");
    other.host = Some("127.0.0.2".into());
    let mut empty = ServerProfile::new("empty");
    empty.host = Some("127.0.0.3".into());
    let forwards = [
        ("first", &dev, Some("web"), 3000, DesiredState::Running),
        ("second", &other, Some("web"), 3001, DesiredState::Stopped),
        ("third", &dev, None, 3002, DesiredState::Stopped),
    ]
    .into_iter()
    .map(|(name, server, group, port, desired_state)| ForwardSpec {
        id: format!("id-{name}"),
        name: name.into(),
        server_id: server.id.clone(),
        group: group.map(str::to_string),
        desired_state,
        tunnel: Tunnel::Dynamic {
            listen: format!("127.0.0.1:{port}").parse().unwrap(),
        },
        connection_mode: ConnectionMode::Shared,
        remote_cleanup: RemoteCleanup::Off,
    })
    .collect();
    let config = Config {
        servers: vec![dev, other, empty],
        forwards,
        ..Default::default()
    };
    config.validate().unwrap();
    config
}

#[test]
fn selections_include_stopped_rules_and_keep_configuration_order() {
    let config = fixture();
    assert_eq!(
        Selection::All.resolve(&config).unwrap(),
        ["id-first", "id-second", "id-third"]
    );
    assert_eq!(
        Selection::Server("dev".into()).resolve(&config).unwrap(),
        ["id-first", "id-third"]
    );
    assert_eq!(
        Selection::Server(config.servers[0].id.clone())
            .resolve(&config)
            .unwrap(),
        ["id-first", "id-third"]
    );
    for selector in ["second", "id-second"] {
        assert_eq!(
            Selection::Forward(selector.into())
                .resolve(&config)
                .unwrap(),
            ["id-second"]
        );
    }
}

#[test]
fn group_and_server_namespaces_are_explicit_and_group_alias_selects_all_members() {
    let config = fixture();
    assert_eq!(
        Selection::Group("web".into()).resolve(&config).unwrap(),
        ["id-first", "id-second"]
    );
    assert_eq!(
        Selection::Forward("web".into()).resolve(&config).unwrap(),
        ["id-first", "id-second"]
    );
    assert_eq!(
        Selection::Server("web".into()).resolve(&config).unwrap(),
        ["id-second"]
    );
    assert!(Selection::Group("first".into()).resolve(&config).is_err());
}

#[test]
fn empty_valid_selections_are_distinct_from_unknown_selections() {
    let config = fixture();
    assert!(
        Selection::Server("empty".into())
            .resolve(&config)
            .unwrap()
            .is_empty()
    );
    assert!(
        Selection::All
            .resolve(&Config::default())
            .unwrap()
            .is_empty()
    );
    for unknown in ["", "missing", "DEV"] {
        assert!(Selection::Server(unknown.into()).resolve(&config).is_err());
        assert!(Selection::Forward(unknown.into()).resolve(&config).is_err());
        assert!(Selection::Group(unknown.into()).resolve(&config).is_err());
    }
}

#[test]
fn renamed_rules_keep_id_selection_but_drop_old_name_selection() {
    let mut config = fixture();
    config.forwards[0].name = "renamed".into();
    assert_eq!(
        Selection::Forward("id-first".into())
            .resolve(&config)
            .unwrap(),
        ["id-first"]
    );
    assert_eq!(
        Selection::Forward("renamed".into())
            .resolve(&config)
            .unwrap(),
        ["id-first"]
    );
    assert!(Selection::Forward("first".into()).resolve(&config).is_err());
}
