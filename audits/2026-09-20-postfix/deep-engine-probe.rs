//! No network: actual supervisors are gated by a semaphore with zero permits.
//! Other tasks are memory-only watch/cancellation stand-ins.
use super::*;
use crate::model::{ForwardSpec, RemoteCleanup, RuntimeState, Tunnel};
use std::sync::atomic::{AtomicUsize, Ordering};

fn fixture_config() -> Config {
    let mut a = ServerProfile::new("server-a"); a.host = Some("127.0.0.1".into());
    let mut b = ServerProfile::new("server-b"); b.host = Some("127.0.0.1".into());
    for server in [&mut a, &mut b] {
        server.ssh_config = Some(std::env::current_dir().unwrap().join("empty_ssh_config"));
        server.known_hosts = Some(std::env::current_dir().unwrap().join("unused_known_hosts"));
    }
    let forwards = (0..4).map(|i| ForwardSpec {
        id: format!("r{i}"), name: format!("r{i}"), group: None,
        server_id: if i < 3 { a.id.clone() } else { b.id.clone() },
        tunnel: Tunnel::Dynamic { listen: ([127,0,0,1], 32000+i).into() },
        desired_state: if i == 2 { DesiredState::Stopped } else { DesiredState::Running },
        connection_mode: ConnectionMode::Shared, remote_cleanup: RemoteCleanup::Off,
    }).collect();
    Config { servers: vec![a,b], forwards, ..Config::default() }
}

fn cache_config(engine: &mut Engine, config: &Config) {
    for server in &config.servers {
        let mut normalized = server.clone(); normalized.name.clear();
        engine.ssh_fingerprints.insert(server.id.clone(), (normalized, "memory-route".into()));
    }
}

async fn fixture() -> (Engine, Config) {
    let config = fixture_config();
    let mut engine = Engine::new();
    engine.connection_limit = Arc::new(Semaphore::new(0));
    cache_config(&mut engine, &config);
    engine.reconcile(&config).await.unwrap();
    // reconcile does not yield; no production task has run yet. Replace them
    // with cooperative memory tasks and keep their actual desired receivers.
    for group in engine.groups.values_mut() {
        group.task.abort();
        let cancel = group.cancel.clone();
        let mut receiver = group.desired.subscribe();
        group.task = tokio::spawn(async move {
            loop { tokio::select! {
                _ = cancel.cancelled() => return,
                changed = receiver.changed() => if changed.is_err() { return; },
            }}
        });
    }
    tokio::task::yield_now().await;
    { let mut values = engine.statuses.lock().unwrap();
      values.get_mut("r0").unwrap().status.state = RuntimeState::Established;
      values.get_mut("r1").unwrap().status.state = RuntimeState::Backoff;
      values.get_mut("r3").unwrap().status.state = RuntimeState::Established; }
    (engine, config)
}

fn generations(engine: &Engine) -> HashMap<String,u64> {
    engine.statuses.lock().unwrap().iter().map(|(id,e)| (id.clone(),e.generation)).collect()
}

struct DropMarker(Arc<AtomicUsize>);
impl Drop for DropMarker { fn drop(&mut self) { self.0.fetch_add(1,Ordering::SeqCst); } }

pub(super) async fn run() -> serde_json::Value {
    let (mut engine, mut config) = fixture().await;
    let before = generations(&engine);
    let group_ids: HashMap<_,_> = engine.groups.iter().map(|(k,g)| (k.clone(), g.task.id())).collect();
    let retry = engine.retry(&["r0".into(), "r1".into(), "r2".into(), "r3".into()]).await.unwrap();
    assert_eq!(retry.affected,["r1"]);
    let after_retry = generations(&engine);
    assert!(after_retry["r1"] > before["r1"]);
    for id in ["r0","r2","r3"] { assert_eq!(after_retry[id],before[id]); }
    let restart = engine.restart(&["r0".into()]).await.unwrap();
    assert_eq!(restart.affected,["r0"]);
    let after_restart = generations(&engine);
    for id in ["r1","r2","r3"] { assert_eq!(after_restart[id],after_retry[id]); }
    for (k,g) in &engine.groups { assert_eq!(g.task.id(),group_ids[k]); assert!(!g.cancel.is_cancelled()); }

    // Many unpolled down/up and target edits: only latest intent/generation is
    // published; old Rule handles cannot overwrite the replacement status.
    let stale_rule = engine.groups.values().flat_map(|g|g.desired.borrow().clone()).find(|r|r.spec.id=="r0").unwrap();
    for index in 0..20 {
        config.forwards[0].desired_state = if index%2==0 {DesiredState::Stopped} else {DesiredState::Running};
        engine.reconcile(&config).await.unwrap();
    }
    let rapid_generation = generations(&engine)["r0"];
    assert!(rapid_generation > stale_rule.generation);
    stale_rule.update(RuntimeState::NeedsAttention,Some("stale completion".into()),99,None);
    assert_eq!(engine.statuses.lock().unwrap()["r0"].status.state, RuntimeState::Starting);
    assert_ne!(engine.statuses.lock().unwrap()["r0"].status.last_error.as_deref(),Some("stale completion"));
    assert_eq!(engine.groups.len(),2);

    let old_tokens: Vec<_> = engine.groups.values().map(|g|g.cancel.clone()).collect();
    config.defaults.retry.max_delay_secs += 1;
    engine.reconcile(&config).await.unwrap();
    assert!(old_tokens.iter().all(CancellationToken::is_cancelled));
    assert_eq!(engine.groups.len(),2);
    let after_defaults = generations(&engine);
    assert!(after_defaults["r0"] > rapid_generation);
    assert!(after_defaults["r3"] > after_restart["r3"]);
    // All actual replacement supervisors are still blocked before transport by
    // the zero-permit semaphore. Remove everything before they can connect.
    config.forwards.clear();
    engine.reconcile(&config).await.unwrap();
    assert!(engine.groups.is_empty());
    assert!(engine.snapshot().await.is_empty());
    tokio::task::yield_now().await;
    engine.reconcile(&config).await.unwrap();
    assert!(engine.retired.is_empty());
    engine.shutdown().await;

    let (mut server_engine, server_config) = fixture().await;
    let other_key = server_engine.groups.iter().find(|(_,g)|g.desired.borrow().iter().any(|r|r.spec.id=="r3")).unwrap().0.clone();
    let other_task = server_engine.groups[&other_key].task.id();
    let other_generation = generations(&server_engine)["r3"];
    let server_restart = server_engine.reconnect_server(&server_config,"server-a").await.unwrap();
    assert_eq!(server_restart.affected,["r0","r1"]);
    assert_eq!(server_restart.skipped[0].id,"r2");
    assert_eq!(server_engine.groups[&other_key].task.id(),other_task);
    assert_eq!(generations(&server_engine)["r3"],other_generation);
    // First reload resolves the synthetic cached route; the next is unchanged.
    server_engine.refresh_ssh_config(&server_config).await.unwrap();
    let refreshed_tasks: HashMap<_,_> = server_engine.groups.iter().map(|(k,g)|(k.clone(),g.task.id())).collect();
    server_engine.refresh_ssh_config(&server_config).await.unwrap();
    for (k,g) in &server_engine.groups { assert_eq!(g.task.id(),refreshed_tasks[k]); }
    server_engine.shutdown().await;

    let mut all_stopped = fixture_config();
    for rule in &mut all_stopped.forwards { rule.desired_state=DesiredState::Stopped; }
    let mut stopped_engine = Engine::new();
    stopped_engine.connection_limit = Arc::new(Semaphore::new(0));
    cache_config(&mut stopped_engine,&all_stopped);
    stopped_engine.reconcile(&all_stopped).await.unwrap();
    tokio::task::yield_now().await;
    assert!(stopped_engine.snapshot().await.iter().all(|s|s.state==RuntimeState::Stopped));
    assert!(stopped_engine.groups.values().all(|g|!g.task.is_finished()));
    let all_stopped_snapshot = stopped_engine.snapshot().await;
    stopped_engine.shutdown().await;

    // Boundary for the completed-task fault: explicit server restart removes
    // the group, unlike rule retry/restart and ordinary reconcile.
    let (mut recovered, recovered_config) = fixture().await;
    let recovery_key = recovered.groups.iter().find(|(_,g)|g.desired.borrow().iter().any(|r|r.spec.id=="r0")).unwrap().0.clone();
    let dead_task = recovered.groups[&recovery_key].task.id();
    recovered.groups.get_mut(&recovery_key).unwrap().task.abort();
    tokio::task::yield_now().await;
    assert!(recovered.groups[&recovery_key].task.is_finished());
    recovered.reconnect_server(&recovered_config,"server-a").await.unwrap();
    let replacement = recovered.groups.values().find(|g|g.desired.borrow().iter().any(|r|r.spec.id=="r0")).unwrap();
    assert_ne!(replacement.task.id(),dead_task);
    assert!(!replacement.task.is_finished());
    recovered.shutdown().await;

    let mut matrix = vec![];
    for alive in [true,false] {
      for state in [RuntimeState::Stopped,RuntimeState::Starting,RuntimeState::Established,RuntimeState::Backoff,RuntimeState::NeedsAttention,RuntimeState::Stopping,RuntimeState::Unverified] {
        for desired in [DesiredState::Running, DesiredState::Stopped] {
          let (mut e, mut c) = fixture().await;
          c.forwards[0].desired_state = desired;
          e.reconcile(&c).await.unwrap();
          let key = e.groups.iter().find(|(_,g)|g.desired.borrow().iter().any(|r|r.spec.id=="r0")).unwrap().0.clone();
          if !alive { e.groups.get_mut(&key).unwrap().task.abort(); tokio::task::yield_now().await; assert!(e.groups[&key].task.is_finished()); }
          e.statuses.lock().unwrap().get_mut("r0").unwrap().status.state = state;
          let task_before = e.groups[&key].task.id();
          let retry_report = e.retry(&["r0".into()]).await.unwrap();
          let restart_report = e.restart(&["r0".into()]).await.unwrap();
          e.reconcile(&c).await.unwrap();
          let task_finished = e.groups[&key].task.is_finished();
          assert_eq!(task_finished,!alive);
          assert_eq!(task_before,e.groups[&key].task.id());
          if desired == DesiredState::Stopped { assert!(retry_report.affected.is_empty()); assert!(restart_report.affected.is_empty()); }
          matrix.push(serde_json::json!({"initial_state":state,"desired":desired,"task_alive":alive,"retry":retry_report,"restart":restart_report,"after_reconcile":e.statuses.lock().unwrap()["r0"].status,"task_still_finished":task_finished,"same_task":true}));
          e.shutdown().await;
        }
      }
    }

    // Timeout fault injection uses only async pending futures and Drop guards.
    // Count total shutdown deadline and observe whether abort is joined.
    tokio::time::pause();
    let mut shutdown_cases = vec![];
    for count in [1,3] {
        let mut e = Engine::new();
        let dropped = Arc::new(AtomicUsize::new(0));
        for _ in 0..count {
            let marker = DropMarker(dropped.clone());
            e.retired.push(tokio::spawn(async move { let _marker = marker; std::future::pending::<()>().await; }));
        }
        tokio::task::yield_now().await;
        let started = tokio::time::Instant::now();
        e.shutdown().await;
        let elapsed = started.elapsed().as_millis();
        let dropped_at_return = dropped.load(Ordering::SeqCst);
        tokio::task::yield_now().await;
        let dropped_after_yield = dropped.load(Ordering::SeqCst);
        assert!((5000*count as u128..=5002*count as u128).contains(&elapsed));
        assert!(dropped_at_return<count);
        assert_eq!(dropped_after_yield,count);
        shutdown_cases.push(serde_json::json!({"noncooperative_tasks":count,"elapsed_virtual_ms":elapsed,"dropped_at_return":dropped_at_return,"dropped_after_yield":dropped_after_yield}));
    }
    let (mut forced_restart, forced_config) = fixture().await;
    let key = forced_restart.groups.iter().find(|(_,g)|g.desired.borrow().iter().any(|r|r.spec.id=="r0")).unwrap().0.clone();
    forced_restart.groups.get_mut(&key).unwrap().task.abort();
    let restart_dropped = Arc::new(AtomicUsize::new(0));
    let marker = DropMarker(restart_dropped.clone());
    forced_restart.groups.get_mut(&key).unwrap().task = tokio::spawn(async move { let _marker = marker; std::future::pending::<()>().await; });
    tokio::task::yield_now().await;
    let restart_start = tokio::time::Instant::now();
    forced_restart.reconnect_server(&forced_config,"server-a").await.unwrap();
    let restart_elapsed = restart_start.elapsed().as_millis();
    let restart_drop_at_return = restart_dropped.load(Ordering::SeqCst);
    assert_eq!(restart_drop_at_return,0);
    assert!((5000..=5002).contains(&restart_elapsed));
    tokio::task::yield_now().await;
    assert_eq!(restart_dropped.load(Ordering::SeqCst),1);
    forced_restart.shutdown().await;
    tokio::time::resume();
    serde_json::json!({
        "scope":"production lifecycle code; zero-permit transport gate; memory watch tasks; completed-task and noncooperative-task injections; virtual clock; no SSH/network/agent/helper/signals",
        "normal_controls":{"retry":retry,"restart":restart,"server_restart":server_restart,"all_stopped_snapshot":all_stopped_snapshot,"all_stopped_supervisors_remain_waiting":true,"unchanged_ssh_reload_keeps_task_identity":true,"server_restart_preserves_other_server_task_and_generation":true,"server_restart_replaces_completed_supervisor":true,"generations_before":before,"after_retry":after_retry,"after_restart":after_restart,"rapid_edits":20,"rapid_final_generation":rapid_generation,"after_defaults":after_defaults,"shared_and_other_server_tasks_preserved":true,"stale_update_ignored":true,"defaults_retired_all_old_groups":true,"empty_rules_removed_all_groups_and_statuses":true,"completed_retired_tasks_pruned":true},
        "state_task_matrix":matrix,
        "shutdown_fault_cases":shutdown_cases,
        "server_restart_fault_case":{"elapsed_virtual_ms":restart_elapsed,"old_task_drop_at_return":restart_drop_at_return,"old_task_drop_after_yield":1}
    })
}
