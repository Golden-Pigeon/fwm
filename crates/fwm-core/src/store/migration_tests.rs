use super::*;

#[test]
fn initialize_current_snapshot_can_be_repeated_without_migration() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(directory.path().into())).unwrap();
    let store = Store::new(paths);
    let config = Config::default();
    store.initialize(&config).unwrap();
    store.initialize(&store.load().unwrap().config).unwrap();
    assert_eq!(store.load().unwrap().config, config);
}
use crate::model::{
    ConnectionMode, DesiredState, ForwardSpec, RemoteCleanup, ServerProfile, Tunnel,
};

#[test]
fn version_one_reverse_rules_gain_verified_recovery_without_losing_ids() {
    let temp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(temp.path().to_owned())).unwrap();
    paths.ensure_dirs().unwrap();
    let mut server = ServerProfile::new("dev");
    server.host = Some("127.0.0.1".into());
    let forward = ForwardSpec {
        id: "rule-existing".into(),
        name: "proxy".into(),
        group: None,
        server_id: server.id.clone(),
        tunnel: Tunnel::Remote {
            listen: "127.0.0.1:17890".parse().unwrap(),
            target: "localhost:7890".parse().unwrap(),
        },
        desired_state: DesiredState::Stopped,
        connection_mode: ConnectionMode::Shared,
        remote_cleanup: RemoteCleanup::Off,
    };
    let legacy = Config {
        schema_version: 1,
        revision: 10,
        servers: vec![server],
        forwards: vec![forward],
        ..Config::default()
    };
    let text = toml::to_string_pretty(&legacy).unwrap();
    fs::write(&paths.config_file, &text).unwrap();
    fs::write(paths.state_dir.join("applied.toml"), &text).unwrap();
    let store = Store::new(paths.clone());
    let loaded = store.load().unwrap();
    assert_eq!(loaded.config.revision, 11);
    assert_eq!(loaded.config.forwards[0].id, "rule-existing");
    assert_eq!(
        loaded.config.forwards[0].remote_cleanup,
        RemoteCleanup::Verified
    );
    assert_eq!(
        loaded.config.forwards[0].connection_mode,
        ConnectionMode::Dedicated
    );
    store.initialize(&loaded.config).unwrap();
    assert_eq!(store.load().unwrap().config, loaded.config);
    assert_eq!(
        fs::read_to_string(paths.state_dir.join("applied.v1.toml")).unwrap(),
        text
    );
    assert!(!store.has_pending_edits().unwrap());
}

#[test]
fn version_two_upgrade_keeps_explicit_cleanup_choice_and_backs_up_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(directory.path().into())).unwrap();
    paths.ensure_dirs().unwrap();
    let mut server = ServerProfile::new("dev");
    server.host = Some("127.0.0.1".into());
    let old = Config {
        schema_version: 2,
        revision: 7,
        servers: vec![server.clone()],
        forwards: vec![ForwardSpec {
            id: "old-rule".into(),
            name: "proxy".into(),
            group: None,
            server_id: server.id,
            tunnel: Tunnel::Remote {
                listen: "127.0.0.1:17890".parse().unwrap(),
                target: "localhost:7890".parse().unwrap(),
            },
            desired_state: DesiredState::Stopped,
            connection_mode: ConnectionMode::Shared,
            remote_cleanup: RemoteCleanup::Off,
        }],
        ..Config::default()
    };
    let original = toml::to_string_pretty(&old).unwrap();
    fs::write(&paths.config_file, &original).unwrap();
    fs::write(paths.state_dir.join("applied.toml"), &original).unwrap();
    let store = Store::new(paths.clone());
    let loaded = store.load().unwrap().config;
    assert_eq!(loaded.schema_version, crate::model::SCHEMA_VERSION);
    assert_eq!(loaded.revision, 8);
    assert_eq!(loaded.forwards[0].remote_cleanup, RemoteCleanup::Off);
    store.initialize(&loaded).unwrap();
    assert_eq!(
        fs::read_to_string(paths.state_dir.join("applied.v2.toml")).unwrap(),
        original
    );
    assert_eq!(store.load().unwrap().config, loaded);
    assert!(!store.has_pending_edits().unwrap());
}

#[test]
fn migration_does_not_overwrite_unapplied_candidate_changes() {
    let temp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(temp.path().to_owned())).unwrap();
    paths.ensure_dirs().unwrap();
    let old = Config {
        schema_version: 1,
        revision: 5,
        ..Config::default()
    };
    fs::write(
        paths.state_dir.join("applied.toml"),
        toml::to_string(&old).unwrap(),
    )
    .unwrap();
    fs::write(&paths.config_file, "unfinished [edit").unwrap();
    let store = Store::new(paths.clone());
    let loaded = store.load().unwrap();
    store.initialize(&loaded.config).unwrap();
    assert_eq!(
        fs::read_to_string(&paths.config_file).unwrap(),
        "unfinished [edit"
    );
    assert!(store.has_pending_edits().unwrap());
    assert_eq!(
        store.load().unwrap().config.schema_version,
        crate::model::SCHEMA_VERSION
    );
}

fn legacy_batches(version: u32) -> Config {
    let mut server = ServerProfile::new("dev");
    server.id = "server-id".into();
    server.host = Some("127.0.0.1".into());
    let forwards = [
        ("web", "web", 3000),
        ("member-3001", "web", 3001),
        ("api-4000-id", "api", 4000),
        ("api-4001-id", "api", 4001),
    ]
    .into_iter()
    .map(|(id, prefix, port)| ForwardSpec {
        id: id.into(),
        name: format!("{prefix}-{port}"),
        group: None,
        server_id: server.id.clone(),
        tunnel: Tunnel::Local {
            listen: ([127, 0, 0, 1], port).into(),
            target: "localhost:80".parse().unwrap(),
        },
        desired_state: DesiredState::Stopped,
        connection_mode: ConnectionMode::Shared,
        remote_cleanup: RemoteCleanup::Off,
    })
    .collect();
    Config {
        schema_version: version,
        revision: 4,
        servers: vec![server],
        forwards,
        ..Default::default()
    }
}

#[test]
fn legacy_group_inference_skips_forward_ids_and_migrates_unrelated_batches() {
    for version in [1, 2] {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(directory.path().into())).unwrap();
        paths.ensure_dirs().unwrap();
        let old = legacy_batches(version);
        let text = toml::to_string_pretty(&old).unwrap();
        fs::write(&paths.config_file, &text).unwrap();
        fs::write(paths.state_dir.join("applied.toml"), &text).unwrap();
        let store = Store::new(paths.clone());
        let loaded = store.load().unwrap().config;
        loaded.validate().unwrap();
        assert_eq!(loaded.schema_version, crate::model::SCHEMA_VERSION);
        assert_eq!(loaded.revision, old.revision + 1);
        for (original, migrated) in old.forwards.iter().zip(&loaded.forwards) {
            assert_eq!(migrated.id, original.id);
            assert_eq!(migrated.name, original.name);
            assert_eq!(migrated.desired_state, original.desired_state);
            assert_eq!(migrated.tunnel, original.tunnel);
            assert_eq!(
                migrated.group.as_deref(),
                if original.name.starts_with("web-") {
                    None
                } else {
                    Some("api")
                }
            );
        }
        // Read-only loading and the eventual durable upgrade both preserve IDs.
        assert_eq!(fs::read_to_string(&paths.config_file).unwrap(), text);
        store.initialize(&loaded).unwrap();
        assert_eq!(store.load().unwrap().config, loaded);
        assert_eq!(
            fs::read_to_string(paths.state_dir.join(format!("applied.v{version}.toml"))).unwrap(),
            text
        );
    }
}

#[test]
fn legacy_group_inference_keeps_the_separate_explicit_server_namespace() {
    let mut config = legacy_batches(2);
    config.forwards[0].id = "first-member".into();
    config.servers[0].id = "web".into();
    config.servers[0].name = "api".into();
    for forward in &mut config.forwards {
        forward.server_id = "web".into();
    }
    config.migrate().unwrap();
    config.validate().unwrap();
    assert_eq!(config.select_group_forwards("web").unwrap().len(), 2);
    assert_eq!(config.select_group_forwards("api").unwrap().len(), 2);
    assert_eq!(config.select_server_forwards("web").unwrap().len(), 4);
}

#[test]
fn legacy_group_inference_preserves_stopped_alternatives_and_existing_group_validity() {
    for version in [1, 2] {
        let mut config = legacy_batches(version);
        config.forwards.truncate(2);
        config.forwards[0].name = "bundle-3000".into();
        config.forwards[1].name = "bundle-03000".into();
        config.forwards[1].tunnel = config.forwards[0].tunnel.clone();
        config.migrate().unwrap();
        config.validate().unwrap();
        assert!(config.forwards.iter().all(|rule| rule.group.is_none()));

        let mut config = legacy_batches(version);
        let mut existing = config.forwards[2].clone();
        existing.id = "already-grouped".into();
        existing.name = "existing-member".into();
        existing.group = Some("api".into());
        config.forwards.push(existing);
        config.migrate().unwrap();
        config.validate().unwrap();
        assert!(config.forwards[2].group.is_none());
        assert!(config.forwards[3].group.is_none());
        assert_eq!(config.forwards[4].group.as_deref(), Some("api"));
    }
}

#[test]
fn preexisting_name_id_collisions_report_both_objects_and_recovery_without_rewriting_files() {
    for kind in ["forward", "server", "group"] {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(directory.path().into())).unwrap();
        paths.ensure_dirs().unwrap();
        let mut config = legacy_batches(2);
        let (owner_id, other_id, other_name) = match kind {
            "forward" => {
                config.forwards[0].name = config.forwards[1].id.clone();
                (
                    config.forwards[0].id.clone(),
                    config.forwards[1].id.clone(),
                    config.forwards[1].name.clone(),
                )
            }
            "server" => {
                let mut other = ServerProfile::new("other-server");
                other.id = "other-server-id".into();
                other.host = Some("127.0.0.1".into());
                config.servers[0].name = other.id.clone();
                let identities = (
                    config.servers[0].id.clone(),
                    other.id.clone(),
                    other.name.clone(),
                );
                config.servers.push(other);
                identities
            }
            _ => {
                config.forwards[1].group = Some(config.forwards[0].id.clone());
                (
                    config.forwards[0].id.clone(),
                    config.forwards[0].id.clone(),
                    config.forwards[0].name.clone(),
                )
            }
        };
        let text = toml::to_string_pretty(&config).unwrap();
        fs::write(&paths.config_file, &text).unwrap();
        fs::write(paths.state_dir.join("applied.toml"), &text).unwrap();
        let error = Store::new(paths.clone()).load().unwrap_err();
        let message = format!("{error:#}");
        for expected in [
            &owner_id,
            &other_id,
            &other_name,
            "config.toml",
            "without changing",
            "config recover --from-candidate",
        ] {
            assert!(
                message.contains(expected),
                "{kind}: missing {expected:?}: {message}"
            );
        }
        assert_eq!(fs::read_to_string(&paths.config_file).unwrap(), text);
        assert_eq!(
            fs::read_to_string(paths.state_dir.join("applied.toml")).unwrap(),
            text
        );
        assert!(!paths.state_dir.join("applied.v2.toml").exists());
    }
}
