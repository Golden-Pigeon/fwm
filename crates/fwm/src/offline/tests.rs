use super::*;
use fwm_core::model::{Config, ServerProfile};

fn fixture() -> (tempfile::TempDir, Paths) {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(directory.path().into())).unwrap();
    Store::new(paths.clone())
        .commit(&Config::default())
        .unwrap();
    (directory, paths)
}

fn add(name: &str) -> Command {
    let mut server = ServerProfile::new(name);
    server.host = Some("127.0.0.1".into());
    Command::PutServer { server }
}

#[tokio::test]
async fn stale_revision_cannot_overwrite_a_newer_offline_commit() {
    let (_directory, paths) = fixture();
    mutate(&paths, add("first"), Some(0)).await.unwrap();
    let before = Store::new(paths.clone()).load().unwrap().config;
    let error = mutate(&paths, add("second"), Some(0)).await.unwrap_err();
    assert_eq!(
        error.downcast_ref::<client::ClientError>().unwrap().code,
        "revision_conflict"
    );
    assert_eq!(Store::new(paths.clone()).load().unwrap().config, before);
}

#[tokio::test]
async fn simultaneous_offline_writers_cannot_lose_updates() {
    let (_directory, paths) = fixture();
    let (first, second) = tokio::join!(
        mutate(&paths, add("first"), Some(0)),
        mutate(&paths, add("second"), Some(0)),
    );
    assert_ne!(first.is_ok(), second.is_ok());
    let config = Store::new(paths.clone()).load().unwrap().config;
    assert_eq!(config.revision, 1);
    assert_eq!(config.servers.len(), 1);
    let error = first.err().or(second.err()).unwrap();
    assert!(error.to_string().contains("revision_conflict"));
}

#[test]
fn unsupported_offline_operation_and_invalid_candidate_do_not_persist() {
    let (_directory, paths) = fixture();
    let before = std::fs::read(paths.state_dir.join("applied.toml")).unwrap();
    for command in [
        Command::Shutdown,
        Command::Retry {
            selection: fwm_api::protocol::Selection::All,
        },
        Command::PutServer {
            server: ServerProfile::new("invalid"),
        },
    ] {
        assert!(apply_locked(&paths, command, None).is_err());
        assert_eq!(
            std::fs::read(paths.state_dir.join("applied.toml")).unwrap(),
            before
        );
    }
}

#[tokio::test]
async fn log_write_failure_does_not_undo_a_successful_config_commit() {
    let (_directory, paths) = fixture();
    std::fs::create_dir(paths.state_dir.join("events.jsonl")).unwrap();
    let response = mutate(&paths, add("saved-despite-log-failure"), Some(0))
        .await
        .unwrap();
    assert!(response.ok);
    let reply: MutationReply = serde_json::from_value(response.data).unwrap();
    assert_eq!(reply.revision, 1);
    assert!(reply.message.contains("Event log write failed"));
    assert!(
        Store::new(paths.clone())
            .load()
            .unwrap()
            .config
            .server("saved-despite-log-failure")
            .is_some()
    );
    assert!(!client::running(&paths).await);
}

#[cfg(unix)]
#[tokio::test]
async fn offline_writer_refuses_symlink_lock_without_modifying_target() {
    let (directory, paths) = fixture();
    let victim = directory.path().join("unrelated");
    std::fs::write(&victim, "must remain intact").unwrap();
    std::os::unix::fs::symlink(&victim, &paths.lock_file).unwrap();
    let error = mutate(&paths, add("dev"), None).await.unwrap_err();
    assert!(error.to_string().contains("symlink"));
    assert_eq!(
        std::fs::read_to_string(victim).unwrap(),
        "must remain intact"
    );
    assert!(Store::new(paths).load().unwrap().config.servers.is_empty());
}

#[tokio::test]
async fn offline_writer_waits_for_owner_lock_and_rechecks_revision() {
    let (_directory, paths) = fixture();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&paths.lock_file)
        .unwrap();
    lock.lock_exclusive().unwrap();
    let pending = mutate(&paths, add("stale"), Some(0));
    tokio::pin!(pending);
    assert!(
        tokio::time::timeout(Duration::from_millis(80), &mut pending)
            .await
            .is_err()
    );
    // Simulate the existing owner's final write before releasing the lock.
    apply_locked(&paths, add("owner"), Some(0)).unwrap();
    FileExt::unlock(&lock).unwrap();
    assert!(
        pending
            .await
            .unwrap_err()
            .to_string()
            .contains("revision_conflict")
    );
    assert_eq!(
        Store::new(paths.clone()).load().unwrap().config.servers[0].name,
        "owner"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn restart_keeps_its_command_when_daemon_becomes_online_while_waiting_for_lock() {
    use fwm_api::{
        codec::{read_frame, write_frame},
        protocol::{Request, Selection},
    };
    let (_directory, paths) = fixture();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&paths.lock_file)
        .unwrap();
    lock.lock_exclusive().unwrap();
    let pending = mutate(
        &paths,
        Command::Restart {
            selection: Selection::Forward("web".into()),
        },
        Some(0),
    );
    tokio::pin!(pending);
    // No RPC listener exists initially. The operation must wait for this owner.
    assert!(
        tokio::time::timeout(Duration::from_millis(80), &mut pending)
            .await
            .is_err()
    );
    let listener = crate::platform::ipc::bind(&paths).unwrap();
    let daemon = tokio::spawn(async move {
        for _ in 0..2 {
            let mut stream = listener.accept().await.unwrap();
            let request: Request = read_frame(&mut stream).await.unwrap().unwrap();
            assert!(matches!(request.command, Command::Ping));
            write_frame(
                &mut stream,
                &Response::success(
                    request.request_id,
                    serde_json::json!({"capabilities":["cli_ux_v5"]}),
                ),
            )
            .await
            .unwrap();
        }
        let mut stream = listener.accept().await.unwrap();
        let request: Request = read_frame(&mut stream).await.unwrap().unwrap();
        assert!(
            matches!(&request.command, Command::Restart { selection: Selection::Forward(name) } if name == "web")
        );
        assert_eq!(request.expected_revision, Some(0));
        write_frame(
            &mut stream,
            &Response::success(request.request_id, serde_json::json!({"restarted":true})),
        )
        .await
        .unwrap();
    });
    let response = tokio::time::timeout(Duration::from_secs(2), &mut pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.data["restarted"], true);
    daemon.await.unwrap();
    FileExt::unlock(&lock).unwrap();
    assert_eq!(Store::new(paths.clone()).load().unwrap().config.revision, 0);
}
