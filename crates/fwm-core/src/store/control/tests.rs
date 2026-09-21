use super::*;
use crate::model::{ConnectionMode, ForwardSpec, RemoteCleanup, ServerProfile, Tunnel};

fn fixture() -> (tempfile::TempDir, Paths, Store, Config) {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(directory.path().to_owned())).unwrap();
    let store = Store::new(paths.clone());
    let mut server = ServerProfile::new("test");
    server.host = Some("127.0.0.1".into());
    let forwards = (0..2)
        .map(|index| ForwardSpec {
            id: format!("rule-{index}"),
            name: format!("web-{index}"),
            group: Some("web".into()),
            server_id: server.id.clone(),
            tunnel: Tunnel::Local {
                listen: ([127, 0, 0, 1], 3000 + index).into(),
                target: "localhost:8080".parse().unwrap(),
            },
            desired_state: DesiredState::Running,
            connection_mode: ConnectionMode::Shared,
            remote_cleanup: RemoteCleanup::Off,
        })
        .collect();
    let config = Config {
        servers: vec![server],
        forwards,
        ..Config::default()
    };
    store.commit(&config).unwrap();
    (directory, paths, store, config)
}

#[test]
fn controls_preserve_unrelated_draft_changes_and_survive_reload() {
    let (_directory, paths, store, mut applied) = fixture();
    let mut draft = applied.clone();
    draft.forwards[1].desired_state = DesiredState::Stopped;
    draft.forwards[1].tunnel = Tunnel::Local {
        listen: "127.0.0.1:3001".parse().unwrap(),
        target: "localhost:9000".parse().unwrap(),
    };
    let text = toml::to_string(&draft).unwrap();
    fs::write(&paths.config_file, &text).unwrap();
    applied.forwards[0].desired_state = DesiredState::Stopped;
    applied.revision += 1;
    let warning = store
        .commit_control_for(&applied, &["rule-0".into()])
        .unwrap()
        .unwrap();
    assert!(warning.contains("pending edits"));
    assert_eq!(fs::read_to_string(&paths.config_file).unwrap(), text);
    assert_eq!(store.load().unwrap().config, applied);
    let mut merged = store.read_candidate().unwrap();
    assert_eq!(merged.forwards[0].desired_state, DesiredState::Stopped);
    assert_eq!(merged.forwards[1], draft.forwards[1]);
    merged.revision = applied.revision + 1;
    store.commit_reload(&merged).unwrap();
    assert_eq!(store.load().unwrap().config, merged);
    assert!(!store.has_pending_edits().unwrap());
    assert!(
        read_overrides(&store.snapshot())
            .unwrap()
            .forwards
            .is_empty()
    );
}

#[test]
fn malformed_draft_is_preserved_and_repaired_old_draft_cannot_resurrect_deleted_rules() {
    let (_directory, paths, store, mut applied) = fixture();
    let original = fs::read_to_string(&paths.config_file).unwrap();
    fs::write(&paths.config_file, "invalid [draft").unwrap();
    applied.forwards.remove(0);
    applied.revision += 1;
    store
        .commit_control_for(&applied, &["rule-0".into()])
        .unwrap();
    assert_eq!(
        fs::read_to_string(&paths.config_file).unwrap(),
        "invalid [draft"
    );
    let restarted = Store::new(paths.clone());
    assert_eq!(restarted.load().unwrap().config, applied);
    assert!(restarted.read_candidate().is_err());
    fs::write(&paths.config_file, original).unwrap();
    let candidate = restarted.read_candidate().unwrap();
    assert_eq!(candidate.forwards.len(), 1);
    assert_eq!(candidate.forwards[0].id, "rule-1");
}

#[test]
fn repeated_down_records_intent_and_a_later_up_supersedes_it() {
    let (_directory, paths, store, mut applied) = fixture();
    applied.forwards[0].desired_state = DesiredState::Stopped;
    store.commit(&applied).unwrap();
    let mut draft = applied.clone();
    draft.forwards[0].desired_state = DesiredState::Running;
    fs::write(&paths.config_file, toml::to_string(&draft).unwrap()).unwrap();
    applied.revision += 1;
    store
        .commit_control_for(&applied, &["rule-0".into()])
        .unwrap();
    assert_eq!(
        store.read_candidate().unwrap().forwards[0].desired_state,
        DesiredState::Stopped
    );
    applied.forwards[0].desired_state = DesiredState::Running;
    applied.revision += 1;
    store
        .commit_control_for(&applied, &["rule-0".into()])
        .unwrap();
    draft.forwards[0].desired_state = DesiredState::Stopped;
    fs::write(&paths.config_file, toml::to_string(&draft).unwrap()).unwrap();
    assert_eq!(
        store.read_candidate().unwrap().forwards[0].desired_state,
        DesiredState::Running
    );
}

#[test]
fn malformed_candidate_cannot_prevent_stop_and_ordinary_edits_still_require_reload() {
    let (_directory, paths, store, mut applied) = fixture();
    fs::write(&paths.config_file, "[broken").unwrap();
    applied.forwards[0].desired_state = DesiredState::Stopped;
    applied.revision += 1;
    assert!(store.commit(&applied).is_err());
    store.commit_control(&applied).unwrap();
    assert_eq!(store.load().unwrap().config, applied);
    assert_eq!(fs::read_to_string(paths.config_file).unwrap(), "[broken");
}

#[test]
fn pending_control_follows_ids_when_a_draft_reassigns_the_old_name() {
    for action in [
        None,
        Some(DesiredState::Stopped),
        Some(DesiredState::Running),
    ] {
        let (_directory, paths, store, mut applied) = fixture();
        if action == Some(DesiredState::Running) {
            for rule in &mut applied.forwards {
                rule.desired_state = DesiredState::Stopped;
            }
            store.commit(&applied).unwrap();
        }
        let mut draft = applied.clone();
        draft.forwards[0].name = "renamed".into();
        draft.forwards[1].name = applied.forwards[0].name.clone();
        fs::write(&paths.config_file, toml::to_string_pretty(&draft).unwrap()).unwrap();
        match action {
            Some(state) => applied.forwards[0].desired_state = state,
            None => {
                applied.forwards.remove(0);
            }
        }
        applied.revision += 1;
        store
            .commit_control_for(&applied, &["rule-0".into()])
            .unwrap();
        let candidate = store.read_candidate().unwrap();
        let untouched = candidate
            .forwards
            .iter()
            .find(|rule| rule.id == "rule-1")
            .unwrap();
        assert_eq!(*untouched, draft.forwards[1]);
        assert_eq!(
            candidate
                .forwards
                .iter()
                .find(|rule| rule.id == "rule-0")
                .map(|rule| rule.desired_state),
            action
        );
        store.commit_reload(&candidate).unwrap();
        assert_eq!(store.load().unwrap().config.forwards, candidate.forwards);
    }
}

#[test]
fn oversized_accumulated_control_records_are_rejected_before_commit() {
    let (_directory, _paths, store, config) = fixture();
    let snapshot = fs::read(store.snapshot()).unwrap();
    let mut overrides = Overrides::default();
    for index in 0..15000 {
        overrides.forwards.insert(
            uuid::Uuid::new_v4().to_string(),
            Intent {
                name: format!("deleted-rule-{index}"),
                desired_state: None,
            },
        );
    }
    let error = store
        .commit_with_overrides(&config, overrides, Some(store.candidate_version().unwrap()))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("including pending control intent exceeds 1 MiB")
    );
    assert_eq!(fs::read(store.snapshot()).unwrap(), snapshot);
    assert_eq!(store.load().unwrap().config, config);
}
