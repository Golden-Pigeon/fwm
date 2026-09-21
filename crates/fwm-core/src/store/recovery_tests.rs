use super::*;
use crate::model::{ConnectionMode, ForwardSpec, RemoteCleanup, ServerProfile, Tunnel};

fn fixture() -> (tempfile::TempDir, Store, Config) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(Paths::new(Some(directory.path().into())).unwrap());
    let mut server = ServerProfile::new("dev");
    server.host = Some("127.0.0.1".into());
    let config = Config {
        servers: vec![server.clone()],
        forwards: (0..2)
            .map(|n| ForwardSpec {
                id: format!("id-{n}"),
                name: format!("web-{n}"),
                group: None,
                server_id: server.id.clone(),
                tunnel: Tunnel::Dynamic {
                    listen: ([127, 0, 0, 1], 31000 + n).into(),
                },
                desired_state: DesiredState::Running,
                connection_mode: ConnectionMode::Shared,
                remote_cleanup: RemoteCleanup::Off,
            })
            .collect(),
        ..Default::default()
    };
    store.commit(&config).unwrap();
    (directory, store, config)
}

#[test]
fn recovery_preserves_readable_stop_delete_intent_and_backs_up_original_bytes() {
    let (_directory, store, config) = fixture();
    let mut draft = config.clone();
    draft.defaults.retry.max_delay_secs = 99;
    let candidate = toml::to_string_pretty(&draft).unwrap();
    fs::write(&store.paths.config_file, &candidate).unwrap();
    let mut controlled = config;
    controlled.revision += 1;
    controlled.forwards.remove(1);
    controlled.forwards[0].desired_state = DesiredState::Stopped;
    store
        .commit_control_for(&controlled, &["id-0".into(), "id-1".into()])
        .unwrap();
    let header = fs::read_to_string(store.snapshot())
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_owned();
    let damaged = format!("{header}\nmalformed = [");
    fs::write(store.snapshot(), &damaged).unwrap();
    let result = store.recover_from_candidate(false).unwrap();
    assert_eq!(result.config.forwards.len(), 1);
    assert_eq!(result.config.forwards[0].id, "id-0");
    assert_eq!(
        result.config.forwards[0].desired_state,
        DesiredState::Stopped
    );
    assert_eq!(result.config.defaults.retry.max_delay_secs, 99);
    assert_eq!(
        fs::read_to_string(result.backup_directory.join("config.toml")).unwrap(),
        candidate
    );
    assert_eq!(
        fs::read_to_string(result.backup_directory.join("applied.toml")).unwrap(),
        damaged
    );
    assert_eq!(store.load().unwrap().config, result.config);
    assert_eq!(store.read_candidate().unwrap(), result.config);
}

#[test]
fn unreadable_intent_requires_explicit_discard_and_never_silently_starts_rules() {
    let (_directory, store, _) = fixture();
    let candidate = fs::read(&store.paths.config_file).unwrap();
    let damaged = "# fwm-control-overrides: {invalid\ninvalid = [";
    fs::write(store.snapshot(), damaged).unwrap();
    let error = store.recover_from_candidate(false).unwrap_err();
    assert!(format!("{error:#}").contains("--discard-unreadable-intent"));
    assert_eq!(fs::read(&store.paths.config_file).unwrap(), candidate);
    assert_eq!(fs::read_to_string(store.snapshot()).unwrap(), damaged);
    assert!(!store.paths.state_dir.join("recovery-backups").exists());
    let result = store.recover_from_candidate(true).unwrap();
    assert!(result.warning.unwrap().contains("explicitly discarded"));
    assert!(
        result
            .config
            .forwards
            .iter()
            .all(|f| f.desired_state == DesiredState::Stopped)
    );
    assert_eq!(
        fs::read_to_string(result.backup_directory.join("applied.toml")).unwrap(),
        damaged
    );
}

#[test]
fn invalid_candidate_is_rejected_before_backups_or_commits_and_recovery_is_repeatable() {
    let (_directory, store, config) = fixture();
    let original = fs::read(store.snapshot()).unwrap();
    fs::write(&store.paths.config_file, "schema_version=3\nwrong_field=1").unwrap();
    assert!(store.recover_from_candidate(false).is_err());
    assert_eq!(fs::read(store.snapshot()).unwrap(), original);
    assert!(!store.paths.state_dir.join("recovery-backups").exists());
    fs::write(
        &store.paths.config_file,
        toml::to_string_pretty(&config).unwrap(),
    )
    .unwrap();
    let first = store.recover_from_candidate(false).unwrap();
    let second = store.recover_from_candidate(false).unwrap();
    assert_ne!(first.backup_directory, second.backup_directory);
    assert!(first.backup_directory.exists());
    assert_eq!(second.config.revision, first.config.revision + 1);
    assert_eq!(second.config.forwards[0].id, first.config.forwards[0].id);
}

#[test]
fn recovery_advances_the_committed_revision_even_from_an_older_candidate() {
    let (_directory, store, mut config) = fixture();
    config.revision = 42;
    store.commit(&config).unwrap();
    config.revision = 0;
    fs::write(
        &store.paths.config_file,
        toml::to_string_pretty(&config).unwrap(),
    )
    .unwrap();
    let recovered = store.recover_from_candidate(false).unwrap();
    assert_eq!(recovered.config.revision, 43);
    // The independent high-water mark still works after the entire applied
    // file is lost, not just when its revision line remains readable.
    fs::remove_file(store.snapshot()).unwrap();
    fs::write(
        &store.paths.config_file,
        toml::to_string_pretty(&config).unwrap(),
    )
    .unwrap();
    assert!(
        store
            .load()
            .unwrap_err()
            .to_string()
            .contains("initialized instance")
    );
    assert!(store.initialize(&config).is_err());
    assert!(store.commit(&config).is_err());
    assert!(store.commit_reload(&config).is_err());
    let recovered = store.recover_from_candidate(false).unwrap();
    assert_eq!(recovered.config.revision, 44);
    assert!(
        recovered
            .config
            .forwards
            .iter()
            .all(|rule| rule.desired_state == DesiredState::Stopped)
    );
}

#[test]
fn committed_revision_maximum_remains_readable_and_overflow_never_replaces_files() {
    let (_directory, store, mut config) = fixture();
    config.revision = i64::MAX as u64 - 1;
    store.commit(&config).unwrap();
    config.revision += 1;
    store.commit(&config).unwrap();
    assert_eq!(store.load().unwrap().config, config);
    let snapshot = fs::read(store.snapshot()).unwrap();
    let candidate = fs::read(&store.paths.config_file).unwrap();
    config.revision += 1;
    config.forwards[0].desired_state = DesiredState::Stopped;
    assert!(
        store
            .commit_control_for(&config, &["id-0".into()])
            .unwrap_err()
            .to_string()
            .contains("revision exhausted")
    );
    assert!(store.initialize(&config).is_err());
    assert!(
        store
            .recover_from_candidate(false)
            .unwrap_err()
            .to_string()
            .contains("revision exhausted")
    );
    assert_eq!(fs::read(store.snapshot()).unwrap(), snapshot);
    assert_eq!(fs::read(&store.paths.config_file).unwrap(), candidate);
}

#[test]
fn absolute_ssh_path_failure_does_not_block_loading_stopping_or_removing_it() {
    let (directory, store, mut config) = fixture();
    let parent = directory.path().join("keys");
    fs::create_dir(&parent).unwrap();
    config.servers[0].identity_files = vec![parent.join("sub/id")];
    store.commit(&config).unwrap();
    fs::remove_dir(&parent).unwrap();
    fs::write(&parent, "no longer a directory").unwrap();
    let mut loaded = store.load().unwrap().config;
    loaded.revision += 1;
    loaded.forwards[0].desired_state = DesiredState::Stopped;
    store.commit_control_for(&loaded, &["id-0".into()]).unwrap();
    loaded.servers[0].identity_files.clear();
    loaded.forwards.clear();
    loaded.revision += 1;
    store.commit(&loaded).unwrap();
    assert_eq!(store.load().unwrap().config, loaded);
}

#[cfg(unix)]
#[test]
fn dangling_candidate_link_is_rejected_by_all_first_write_paths() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(directory.path().into())).unwrap();
    paths.ensure_dirs().unwrap();
    let target = directory.path().join("missing");
    std::os::unix::fs::symlink(&target, &paths.config_file).unwrap();
    let store = Store::new(paths.clone());
    assert!(store.load().unwrap_err().to_string().contains("symlink"));
    assert!(store.initialize(&Config::default()).is_err());
    assert!(store.commit(&Config::default()).is_err());
    assert!(
        fs::symlink_metadata(&paths.config_file)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(!store.snapshot().exists());
    assert!(!target.exists());
}

#[test]
fn missing_legacy_snapshot_requires_explicit_recovery() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(directory.path().into())).unwrap();
    paths.ensure_dirs().unwrap();
    fs::write(paths.state_dir.join("recovery.json"), "{}").unwrap();
    fs::write(
        &paths.config_file,
        toml::to_string_pretty(&Config::default()).unwrap(),
    )
    .unwrap();
    let store = Store::new(paths);
    assert!(
        store
            .load()
            .unwrap_err()
            .to_string()
            .contains("config recover")
    );
    assert!(store.initialize(&Config::default()).is_err());
}
