use super::*;
use crate::test_support::{Peer, Reply};
use fwm_core::{model::Config, store::Store};

fn saved(config: Config) -> Response {
    Response::success(
        "saved".into(),
        serde_json::to_value(MutationReply {
            revision: config.revision,
            config,
            message: "saved; daemon remains stopped.".into(),
            operation: None,
        })
        .unwrap(),
    )
}
fn snapshot(state: &str) -> Value {
    json!({"daemon_instance_id":"test","config_revision":7,"forwards":[{
        "id":"rule","name":"web","server":"dev","kind":"local","listen":"127.0.0.1:3000","target":"localhost:3000",
        "desired_state":"running","state":state,"retry_count":0,"next_retry_unix_ms":null,"last_error":null,"active_connections":0
    }]})
}

#[tokio::test]
async fn saved_configuration_survives_start_failure_and_error_exposes_saved_true() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().into())).unwrap();
    let config = Config {
        revision: 7,
        ..Default::default()
    };
    Store::new(paths.clone()).commit(&config).unwrap();
    std::fs::create_dir(&paths.log_file).unwrap();
    let error = start_saved(&paths, saved(config.clone()))
        .await
        .unwrap_err();
    let error = error.downcast_ref::<CompletionError>().unwrap();
    assert_eq!(error.result["data"]["saved"], true);
    assert_eq!(error.result["data"]["ready"], false);
    assert_eq!(error.result["data"]["revision"], 7);
    assert_eq!(Store::new(paths).load().unwrap().config, config);
}

#[tokio::test]
async fn readiness_query_failure_keeps_last_snapshot_and_saved_revision() {
    for reply in [
        Reply::Disconnect,
        Reply::InvalidJson,
        Reply::Data(json!({"bad":"snapshot"})),
        Reply::Failure("runtime_error"),
    ] {
        let peer = Peer::new(vec![Reply::Data(snapshot("starting")), reply]);
        let response = saved(Config {
            revision: 7,
            ..Default::default()
        });
        let error = mutation(
            &peer.paths,
            response,
            &["rule".into()],
            Some(Duration::from_secs(2)),
            true,
        )
        .await
        .unwrap_err();
        let error = error.downcast_ref::<CompletionError>().unwrap();
        assert_eq!(error.code, "wait_failed");
        assert_eq!(error.result["ok"], false);
        assert_eq!(error.result["data"]["saved"], true);
        assert_eq!(error.result["data"]["revision"], 7);
        assert_eq!(
            error.result["data"]["runtime"]["forwards"][0]["state"],
            "starting"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn wait_deadline_interrupts_a_stalled_status_request() {
    let mut peer = Peer::new(vec![Reply::Stall]);
    let paths = peer.paths.clone();
    let task = tokio::spawn(async move {
        mutation(
            &paths,
            saved(Config::default()),
            &["rule".into()],
            Some(Duration::from_secs(2)),
            true,
        )
        .await
    });
    peer.received.recv().await.unwrap();
    tokio::time::advance(Duration::from_secs(3)).await;
    let error = task.await.unwrap().unwrap_err();
    let error = error.downcast_ref::<CompletionError>().unwrap();
    assert_eq!(error.code, "wait_timeout");
    assert_eq!(error.result["data"]["saved"], true);
}

#[tokio::test]
async fn missing_selected_rule_cannot_be_mistaken_for_ready() {
    let peer = Peer::new(vec![
        Reply::Data(snapshot("established")),
        Reply::Disconnect,
    ]);
    let error = mutation(
        &peer.paths,
        saved(Config::default()),
        &["different-rule".into()],
        Some(Duration::from_secs(1)),
        true,
    )
    .await
    .unwrap_err();
    let error = error.downcast_ref::<CompletionError>().unwrap();
    assert_eq!(error.code, "wait_failed");
    assert!(
        error.result["data"]["runtime"]["forwards"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
