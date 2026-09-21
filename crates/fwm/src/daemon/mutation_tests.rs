use super::tests::Fixture;
use fwm_api::protocol::{Command, Request, Selection};
use fwm_core::{
    model::{Config, DesiredState, ServerProfile},
    paths::Paths,
};

async fn group(fixture: &Fixture) -> Config {
    let mut forwards = vec![
        fixture.rule("web-3000", 3000),
        fixture.rule("web-3001", 3001),
    ];
    for forward in &mut forwards {
        forward.group = Some("web".into());
    }
    let response = fixture
        .send(Request::new(
            "create",
            Command::CreateForwards {
                forwards,
                server: None,
            },
        ))
        .await;
    assert!(response.ok, "{response:?}");
    assert_eq!(
        response.data["message"],
        "2 forward(s) saved (running: 0, stopped: 2)"
    );
    fixture.state.lock().await.config.clone()
}

#[tokio::test]
async fn group_delete_is_one_idempotent_commit_and_cannot_be_revived_by_a_broken_draft() {
    let fixture = Fixture::new().await;
    let before = group(&fixture).await;
    let paths = Paths::new(Some(fixture._directory.path().into())).unwrap();
    std::fs::write(&paths.config_file, "unfinished [draft").unwrap();
    let mut request = Request::new(
        "remove-group",
        Command::RemoveForwards {
            selection: Selection::Group("web".into()),
        },
    );
    request.expected_revision = Some(before.revision);
    let response = fixture.send(request.clone()).await;
    assert!(response.ok, "{response:?}");
    let again = fixture.send(request).await;
    assert_eq!(response.data, again.data);
    assert_eq!(
        std::fs::read_to_string(&paths.config_file).unwrap(),
        "unfinished [draft"
    );
    let mut state = fixture.state.lock().await;
    assert!(state.config.forwards.is_empty());
    assert_eq!(state.config.revision, before.revision + 1);
    assert_eq!(state.store.load().unwrap().config, state.config);
    std::fs::write(&paths.config_file, toml::to_string(&before).unwrap()).unwrap();
    assert!(state.store.read_candidate().unwrap().forwards.is_empty());
    state.engine.shutdown().await;
}

#[tokio::test]
async fn no_op_group_down_protects_stopped_rules_from_pending_start_edits() {
    let fixture = Fixture::new().await;
    let mut draft = group(&fixture).await;
    let paths = Paths::new(Some(fixture._directory.path().into())).unwrap();
    for forward in &mut draft.forwards {
        forward.desired_state = DesiredState::Running;
    }
    let original = toml::to_string(&draft).unwrap();
    std::fs::write(&paths.config_file, &original).unwrap();
    let response = fixture
        .send(Request::new(
            "down",
            Command::SetDesired {
                selection: Selection::Forward("web".into()),
                state: DesiredState::Stopped,
            },
        ))
        .await;
    assert!(response.ok, "{response:?}");
    assert_eq!(
        std::fs::read_to_string(&paths.config_file).unwrap(),
        original
    );
    let mut state = fixture.state.lock().await;
    assert!(
        state
            .store
            .read_candidate()
            .unwrap()
            .forwards
            .iter()
            .all(|rule| rule.desired_state == DesiredState::Stopped)
    );
    state.engine.shutdown().await;
}

#[tokio::test]
async fn group_edit_and_new_server_are_atomic_and_all_ids_must_exist() {
    let fixture = Fixture::new().await;
    let original = group(&fixture).await;
    let mut server = ServerProfile::new("new-alias");
    server.ssh_alias = Some("new-alias".into());
    let mut changed = original.forwards.clone();
    for rule in &mut changed {
        rule.server_id = server.id.clone();
    }
    let mut invalid = changed.clone();
    invalid[1].id = "missing-id".into();
    let failure = fixture
        .send(Request::new(
            "bad-edit",
            Command::PutForwardsWithServer {
                forwards: invalid,
                server: Some(server.clone()),
            },
        ))
        .await;
    assert_eq!(failure.error.unwrap().code, "not_found");
    assert_eq!(fixture.state.lock().await.config, original);
    let mut request = Request::new(
        "edit",
        Command::PutForwardsWithServer {
            forwards: changed.clone(),
            server: Some(server.clone()),
        },
    );
    request.expected_revision = Some(original.revision);
    let response = fixture.send(request.clone()).await;
    assert!(response.ok, "{response:?}");
    assert_eq!(fixture.send(request).await.data, response.data);
    let mut state = fixture.state.lock().await;
    assert_eq!(state.config.revision, original.revision + 1);
    assert_eq!(state.config.forwards, changed);
    assert_eq!(state.config.server(&server.id), Some(&server));
    assert_eq!(state.store.load().unwrap().config, state.config);
    state.engine.shutdown().await;
}

#[tokio::test]
async fn restart_stopped_group_persists_running_intent_and_returns_actual_selection() {
    let fixture = Fixture::new().await;
    let before = group(&fixture).await;
    let mut request = Request::new(
        "restart-group",
        Command::Restart {
            selection: Selection::Group("web".into()),
        },
    );
    request.expected_revision = Some(before.revision);
    let response = fixture.send(request.clone()).await;
    assert!(response.ok, "{response:?}");
    let ids = before
        .forwards
        .iter()
        .map(|forward| forward.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        response.data["operation"]["affected"],
        serde_json::json!(ids)
    );
    assert_eq!(fixture.send(request).await.data, response.data);
    let mut state = fixture.state.lock().await;
    assert!(
        state
            .config
            .forwards
            .iter()
            .all(|forward| forward.desired_state == DesiredState::Running)
    );
    assert_eq!(state.config.revision, before.revision + 1);
    state.engine.shutdown().await;
}
