//! Pure-memory lifecycle fixture. The group task only waits for cancellation.
//! This file is appended only to a temporary copy of fwm-core.
use super::*;
use crate::model::{ForwardSpec, RemoteCleanup, RetryPolicy, Tunnel};

fn fixture() -> (Engine, Config, Rule) {
    let mut server = ServerProfile::new("memory-fixture");
    server.host = Some("127.0.0.1".into());
    let spec = ForwardSpec {
        id: "memory-rule".into(), name: "memory-rule".into(), group: None,
        server_id: server.id.clone(),
        tunnel: Tunnel::Dynamic { listen: "127.0.0.1:1080".parse().unwrap() },
        desired_state: DesiredState::Running,
        connection_mode: ConnectionMode::Shared, remote_cleanup: RemoteCleanup::Off,
    };
    let config = Config { servers: vec![server.clone()], forwards: vec![spec.clone()], ..Config::default() };
    let mut engine = Engine::new();
    let mut normalized = server.clone();
    normalized.name.clear();
    let fingerprint = "pure-memory-cached-fingerprint".to_string();
    let key = serde_json::to_string(&(&normalized, &config.defaults.retry, Option::<&String>::None, &fingerprint)).unwrap();
    engine.ssh_fingerprints.insert(server.id.clone(), (normalized, fingerprint));
    engine.generation = 1;
    engine.statuses.lock().unwrap().insert(spec.id.clone(), StatusEntry {
        generation: 1, connection_key: key.clone(), spec: spec.clone(),
        status: ForwardStatus {
            id: spec.id.clone(), name: spec.name.clone(), group: None, server: server.name.clone(),
            kind: "dynamic".into(), listen: "127.0.0.1:1080".into(), target: None,
            desired_state: DesiredState::Running, state: RuntimeState::Established,
            retry_count: 0, next_retry_unix_ms: None, last_error: None, active_connections: 0,
        },
    });
    let rule = Rule { spec, generation: 1, statuses: engine.statuses.clone(), events: engine.events.clone() };
    let (desired, _) = watch::channel(vec![rule.clone()]);
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move { task_cancel.cancelled().await });
    engine.groups.insert(key, Group { desired, cancel, task });
    (engine, config, rule)
}

pub(super) async fn run() -> serde_json::Value {
    let (mut engine, mut config, old) = fixture();
    let active_a = old.connection_opened();
    let active_b = old.connection_opened();
    let before = engine.snapshot().await;
    let original_generation = engine.generation;
    let unknown = engine.restart(&["memory-rule".into(), "missing".into()]).await.unwrap_err().to_string();
    assert_eq!(engine.generation, original_generation);
    let healthy_skip = engine.retry(&["memory-rule".into()]).await.unwrap();
    assert_eq!(healthy_skip.skipped[0].reason, "already_established");
    config.forwards[0].name = "renamed".into();
    engine.reconcile(&config).await.unwrap();
    assert_eq!(engine.generation, original_generation);
    assert_eq!(engine.snapshot().await[0].active_connections, 2);
    let restarted = engine.restart(&["memory-rule".into(), "memory-rule".into()]).await.unwrap();
    assert_eq!(restarted.affected, ["memory-rule"]);
    let after_restart = engine.snapshot().await;
    assert_eq!(after_restart[0].active_connections, 2);
    drop(active_a);
    drop(active_b);
    let after_old_streams_closed = engine.snapshot().await;
    assert_eq!(after_old_streams_closed[0].active_connections, 2);
    let new_rule = engine.groups.values().next().unwrap().desired.borrow()[0].clone();
    new_rule.update(RuntimeState::Established, None, 0, None);
    let new_guard = new_rule.connection_opened();
    assert_eq!(engine.snapshot().await[0].active_connections, 3);
    drop(new_guard);
    let after_new_stream_closed = engine.snapshot().await;
    assert_eq!(after_new_stream_closed[0].active_connections, 2);
    engine.shutdown().await;
    let shutdown = engine.snapshot().await;
    assert_eq!(shutdown[0].active_connections, 0);

    let (mut stopped_engine, mut stopped_config, old_rule) = fixture();
    let old_guard = old_rule.connection_opened();
    stopped_config.forwards[0].desired_state = DesiredState::Stopped;
    stopped_engine.reconcile(&stopped_config).await.unwrap();
    drop(old_guard);
    let stopped = stopped_engine.snapshot().await;
    assert_eq!(stopped[0].active_connections, 0);
    let stopped_retry = stopped_engine.retry(&["memory-rule".into()]).await.unwrap();
    assert!(stopped_retry.affected.is_empty());
    assert_eq!(stopped_retry.skipped[0].reason, "stopped");
    let stopped_restart = stopped_engine.restart(&["memory-rule".into()]).await.unwrap();
    assert!(stopped_restart.affected.is_empty());
    stopped_engine.shutdown().await;

    let (mut metadata_engine, mut metadata_config, metadata_rule) = fixture();
    let guard = metadata_rule.connection_opened();
    metadata_config.forwards[0].group = Some("new-group".into());
    metadata_engine.reconcile(&metadata_config).await.unwrap();
    drop(guard);
    let metadata = metadata_engine.snapshot().await;
    assert_eq!(metadata[0].active_connections, 0);
    metadata_engine.shutdown().await;

    // Final focused sweep: an ordinary listener backoff is cancelled by stop,
    // and an old-generation completion does not restore a running status.
    tokio::time::pause();
    let (mut backoff_engine, mut backoff_config, backoff_rule) = fixture();
    let backoff_cancel = CancellationToken::new();
    let task_cancel = backoff_cancel.clone();
    let task_rule = backoff_rule.clone();
    let policy = RetryPolicy::default();
    let backoff_task = tokio::spawn(async move {
        forward::backoff(&task_rule, &task_cancel, &policy, 2, "memory listener unavailable".into()).await
    });
    tokio::task::yield_now().await;
    let during_backoff = backoff_engine.snapshot().await;
    assert_eq!(during_backoff[0].state, RuntimeState::Backoff);
    assert_eq!(during_backoff[0].retry_count, 2);
    assert!(during_backoff[0].next_retry_unix_ms.is_some());
    backoff_config.forwards[0].desired_state = DesiredState::Stopped;
    backoff_engine.reconcile(&backoff_config).await.unwrap();
    backoff_cancel.cancel();
    assert!(!backoff_task.await.unwrap());
    tokio::time::advance(Duration::from_secs(60)).await;
    backoff_rule.update(RuntimeState::Established, None, 0, None);
    let after_backoff_stop = backoff_engine.snapshot().await;
    assert_eq!(after_backoff_stop[0].state, RuntimeState::Stopped);
    assert_eq!(after_backoff_stop[0].retry_count, 0);
    assert!(after_backoff_stop[0].next_retry_unix_ms.is_none());
    backoff_engine.shutdown().await;
    tokio::time::resume();

    serde_json::json!({
        "scope": "production lifecycle/reconcile/state functions with pure-memory group tasks; no SSH, sockets, agent, helper, or process signals",
        "restart_count_leak": { "before": before, "restart_report": restarted, "after_restart": after_restart, "after_old_streams_closed": after_old_streams_closed, "after_new_stream_closed": after_new_stream_closed, "after_shutdown": shutdown },
        "controls": { "unknown_selection": unknown, "unknown_selection_atomic": true, "healthy_retry": healthy_skip, "metadata_generation_unchanged": true, "metadata_old_guard_decrements": metadata, "stopped_after_drop": stopped, "stopped_retry": stopped_retry, "stopped_restart": stopped_restart },
        "final_focused_sweep": { "new_findings": 0, "during_backoff": during_backoff, "after_stop_and_60_seconds_virtual_time": after_backoff_stop, "old_generation_update_ignored": true }
    })
}
