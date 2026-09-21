use super::*;
use crate::test_support::{Peer, Reply};
use serde_json::json;

#[tokio::test(start_paused = true)]
async fn stalled_ipc_reports_timeout_without_retrying_an_uncertain_mutation() {
    let mut peer = Peer::new(vec![Reply::Stall]);
    let paths = peer.paths.clone();
    let task = tokio::spawn(async move { request(&paths, Command::Status).await });
    peer.received.recv().await.unwrap();
    tokio::time::advance(Duration::from_secs(46)).await;
    let error = task.await.unwrap().unwrap_err();
    let error = error.downcast_ref::<ClientError>().unwrap();
    assert_eq!(error.code, "ipc_timeout");
    assert!(error.message.contains("may still complete"));
    assert!(peer.received.try_recv().is_err());
}

#[tokio::test]
async fn disconnect_corrupt_frame_and_missing_error_are_not_successes() {
    for reply in [Reply::Disconnect, Reply::InvalidJson, Reply::MissingError] {
        let peer = Peer::new(vec![reply]);
        assert!(request(&peer.paths, Command::Status).await.is_err());
    }
    let peer = Peer::new(vec![Reply::Failure("storage_error")]);
    let error = request(&peer.paths, Command::Status).await.unwrap_err();
    assert_eq!(
        error.downcast_ref::<ClientError>().unwrap().code,
        "storage_error"
    );
}

#[tokio::test]
async fn unsupported_daemon_is_rejected_before_the_mutation_is_sent() {
    let mut peer = Peer::new(vec![Reply::Data(json!({"capabilities":["cli_ux_v3"]}))]);
    let error = send(
        &peer.paths,
        Command::SetDesired {
            selection: fwm_api::protocol::Selection::All,
            state: fwm_core::model::DesiredState::Running,
        },
        Some(7),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<ClientError>().unwrap().code,
        "daemon_upgrade_required"
    );
    assert!(matches!(
        peer.received.recv().await.unwrap().command,
        Command::Ping
    ));
    assert!(peer.received.try_recv().is_err());
}

#[tokio::test]
async fn reload_requires_the_daemon_that_refreshes_ssh_configuration() {
    let mut peer = Peer::new(vec![Reply::Data(json!({"capabilities":["cli_ux_v4"]}))]);
    let error = request(&peer.paths, Command::Reload).await.unwrap_err();
    assert_eq!(
        error.downcast_ref::<ClientError>().unwrap().code,
        "daemon_upgrade_required"
    );
    assert!(matches!(
        peer.received.recv().await.unwrap().command,
        Command::Ping
    ));
    assert!(
        peer.received.try_recv().is_err(),
        "old daemon must not receive a misleading successful reload"
    );
}

#[tokio::test(start_paused = true)]
async fn startup_readiness_is_bounded_and_returns_the_log_location() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().into())).unwrap();
    let error = wait_running(&paths, Duration::from_secs(5))
        .await
        .unwrap_err();
    let error = error.downcast_ref::<ClientError>().unwrap();
    assert_eq!(error.code, "daemon_unavailable");
    assert!(
        error
            .message
            .contains(&paths.log_file.display().to_string())
    );
    assert!(!paths.state_dir.exists());
}

#[tokio::test]
async fn startup_failure_does_not_spawn_when_diagnostic_log_cannot_be_opened() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().into())).unwrap();
    paths.ensure_dirs().unwrap();
    std::fs::create_dir(&paths.log_file).unwrap();
    let error = ensure_running(&paths).await.unwrap_err();
    assert!(format!("{error:#}").contains("opening daemon log"));
    assert!(!running(&paths).await.unwrap());
}

#[tokio::test]
async fn status_view_requires_an_atomic_snapshot_capability_before_sending() {
    let mut peer = Peer::new(vec![Reply::Data(json!({"capabilities":["cli_ux_v5"]}))]);
    let error = request(&peer.paths, Command::StatusView { selection: None })
        .await
        .unwrap_err();
    let error = error.downcast_ref::<ClientError>().unwrap();
    assert_eq!(error.code, "daemon_upgrade_required");
    assert!(error.message.contains("atomic_status_view"));
    assert!(error.message.contains("restart"));
    assert!(matches!(
        peer.received.recv().await.unwrap().command,
        Command::Ping
    ));
    assert!(peer.received.try_recv().is_err());
}

#[tokio::test]
async fn supported_atomic_status_view_is_sent_with_its_selection() {
    let expected = json!({"config":{"revision":3},"snapshot":{"config_revision":3}});
    let mut peer = Peer::new(vec![
        Reply::Data(json!({"capabilities":["atomic_status_view"]})),
        Reply::Data(expected.clone()),
    ]);
    let response = request(
        &peer.paths,
        Command::StatusView {
            selection: Some(fwm_api::protocol::Selection::Forward("chosen".into())),
        },
    )
    .await
    .unwrap();
    assert_eq!(response.data, expected);
    assert!(matches!(
        peer.received.recv().await.unwrap().command,
        Command::Ping
    ));
    assert!(matches!(peer.received.recv().await.unwrap().command,
        Command::StatusView { selection: Some(fwm_api::protocol::Selection::Forward(id)) } if id == "chosen"));
}

#[cfg(unix)]
#[tokio::test]
async fn authentication_failure_cannot_be_treated_as_a_stopped_or_ready_daemon() {
    use std::os::unix::fs::PermissionsExt;

    let (_directory, paths) = crate::test_support::untrusted_ipc_paths();
    for error in [
        running(&paths).await.unwrap_err(),
        presence(&paths).await.unwrap_err(),
        ensure_running(&paths).await.unwrap_err(),
        wait_running(&paths, Duration::from_secs(5))
            .await
            .unwrap_err(),
    ] {
        assert!(ipc::is_authentication_error(&error), "{error:#}");
    }
    assert!(!paths.config_dir.exists(), "no daemon may be launched");
    assert_eq!(
        std::fs::metadata(paths.ipc_path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o777,
        "readiness checks must not change directory permissions"
    );
}
