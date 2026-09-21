use super::*;
use crate::model::{ForwardSpec, RemoteCleanup, Tunnel};

fn fixture() -> (tempfile::TempDir, Config, Engine) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ssh_config");
    std::fs::write(
        &path,
        "Host *\n IdentityAgent none\n GlobalKnownHostsFile none\n",
    )
    .unwrap();
    let mut server = ServerProfile::new("before");
    server.host = Some("127.0.0.1".into());
    server.ssh_config = Some(path);
    let config = Config {
        forwards: vec![ForwardSpec {
            id: "rule".into(),
            name: "rule".into(),
            group: None,
            server_id: server.id.clone(),
            tunnel: Tunnel::Dynamic {
                listen: "127.0.0.1:1080".parse().unwrap(),
            },
            desired_state: DesiredState::Running,
            connection_mode: ConnectionMode::Shared,
            remote_cleanup: RemoteCleanup::Off,
        }],
        servers: vec![server],
        ..Config::default()
    };
    let mut engine = Engine::new();
    engine.connection_limit = Arc::new(Semaphore::new(0));
    (directory, config, engine)
}

#[tokio::test]
async fn metadata_and_unrelated_servers_do_not_wake_failed_groups() {
    let (_directory, mut config, mut engine) = fixture();
    engine.reconcile(&config).await.unwrap();
    let mut updates = engine.groups.values().next().unwrap().desired.subscribe();
    engine
        .statuses
        .lock()
        .unwrap()
        .get_mut("rule")
        .unwrap()
        .status
        .state = RuntimeState::NeedsAttention;
    config.forwards[0].name = "renamed".into();
    config.servers[0].name = "renamed-server".into();
    let mut unused = ServerProfile::new("unused");
    unused.host = Some("unused.invalid".into());
    unused.ssh_config = Some("/does/not/exist".into());
    config.servers.push(unused);
    engine.reconcile(&config).await.unwrap();
    assert!(!updates.has_changed().unwrap());
    assert_eq!(updates.borrow().first().unwrap().spec.name, "renamed");
    assert_eq!(
        engine.snapshot().await[0].state,
        RuntimeState::NeedsAttention
    );
    config.forwards[0].desired_state = DesiredState::Stopped;
    engine.reconcile(&config).await.unwrap();
    assert!(updates.has_changed().unwrap());
    updates.borrow_and_update();
    engine.shutdown().await;
}

#[tokio::test]
async fn restart_and_retry_clear_old_generation_counts() {
    let (_directory, config, mut engine) = fixture();
    engine.reconcile(&config).await.unwrap();
    for retry in [false, true] {
        let old = engine.groups.values().next().unwrap().desired.borrow()[0].clone();
        let guards = [old.connection_opened(), old.connection_opened()];
        engine
            .statuses
            .lock()
            .unwrap()
            .get_mut("rule")
            .unwrap()
            .status
            .state = RuntimeState::Backoff;
        if retry {
            engine.retry(&["rule".into()]).await.unwrap();
        } else {
            engine.restart(&["rule".into()]).await.unwrap();
        }
        assert_eq!(engine.snapshot().await[0].active_connections, 0);
        let new = engine.groups.values().next().unwrap().desired.borrow()[0].clone();
        let current = new.connection_opened();
        drop(guards);
        assert_eq!(engine.snapshot().await[0].active_connections, 1);
        drop(current);
        assert_eq!(engine.snapshot().await[0].active_connections, 0);
    }
    engine.shutdown().await;
}

#[tokio::test]
async fn completed_supervisors_are_replaced_by_retry_restart_and_reconcile() {
    for action in ["retry", "restart", "reconcile"] {
        let (_directory, config, mut engine) = fixture();
        engine.reconcile(&config).await.unwrap();
        let key = engine.groups.keys().next().unwrap().clone();
        let task_id = engine.groups[&key].task.id();
        engine.groups[&key].task.abort();
        while !engine.groups[&key].task.is_finished() {
            tokio::task::yield_now().await;
        }
        engine
            .statuses
            .lock()
            .unwrap()
            .get_mut("rule")
            .unwrap()
            .status
            .state = RuntimeState::Established;
        match action {
            "retry" => assert_eq!(
                engine.retry(&["rule".into()]).await.unwrap().affected,
                ["rule"]
            ),
            "restart" => assert_eq!(
                engine.restart(&["rule".into()]).await.unwrap().affected,
                ["rule"]
            ),
            _ => engine.reconcile(&config).await.unwrap(),
        }
        assert_ne!(task_id, engine.groups[&key].task.id());
        assert!(!engine.groups[&key].task.is_finished());
        engine.shutdown().await;
    }
}

#[tokio::test(start_paused = true)]
async fn shutdown_uses_one_deadline_and_joins_aborted_futures() {
    struct Dropped(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut engine = Engine::new();
    for _ in 0..3 {
        let guard = Dropped(dropped.clone());
        engine.retired.push(tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        }));
    }
    tokio::task::yield_now().await;
    let started = tokio::time::Instant::now();
    engine.shutdown().await;
    assert!(started.elapsed() < Duration::from_secs(6));
    assert_eq!(dropped.load(std::sync::atomic::Ordering::SeqCst), 3);
}

#[tokio::test]
async fn stopping_queued_connection_never_starts_tcp_after_permit_arrives() {
    let (_directory, mut config, mut engine) = fixture();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    config.servers[0].port = Some(listener.local_addr().unwrap().port());
    engine.reconcile(&config).await.unwrap();
    tokio::task::yield_now().await;
    config.forwards[0].desired_state = DesiredState::Stopped;
    engine.reconcile(&config).await.unwrap();
    engine.connection_limit.add_permits(1);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
    assert_eq!(engine.snapshot().await[0].state, RuntimeState::Stopped);
    engine.shutdown().await;
}

#[tokio::test]
async fn stopping_during_handshake_closes_pending_transport() {
    use tokio::io::AsyncReadExt;
    let (_directory, mut config, mut engine) = fixture();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    config.servers[0].port = Some(listener.local_addr().unwrap().port());
    engine.connection_limit.add_permits(1);
    engine.reconcile(&config).await.unwrap();
    let (mut transport, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
        .await
        .unwrap()
        .unwrap();
    config.forwards[0].desired_state = DesiredState::Stopped;
    engine.reconcile(&config).await.unwrap();
    let mut received = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), transport.read_to_end(&mut received))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(engine.snapshot().await[0].state, RuntimeState::Stopped);
    engine.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn unused_servers_never_read_ssh_configuration_fifos() {
    let (_directory, mut config, mut engine) = fixture();
    let fifo = _directory.path().join("fifo");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    config.servers[0].ssh_config = Some(fifo);
    config.forwards.clear();
    engine.reconcile(&config).await.unwrap();
    assert!(engine.ssh_fingerprints.is_empty());
    engine.shutdown().await;
}

#[test]
fn extreme_time_intervals_are_rejected_before_spawning_connections() {
    let mut config = Config::default();
    config.defaults.retry.keepalive_interval_secs = i64::MAX as u64;
    assert!(config.validate().unwrap_err().contains("one year"));
    config.defaults.retry.keepalive_interval_secs = 31_536_000;
    config.validate().unwrap();
}
