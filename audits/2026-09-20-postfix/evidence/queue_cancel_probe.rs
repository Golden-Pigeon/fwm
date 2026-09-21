//! Local TCP fixture only: stalled SSH handshakes expose the attempt queue.
use fwm_core::{engine::Engine, model::*};
use std::{sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}, time::Duration};

#[tokio::main]
async fn main() {
    let directory = tempfile::tempdir().unwrap();
    let ssh_config = directory.path().join("ssh.conf");
    std::fs::write(&ssh_config, "Host *\n IdentityAgent none\n GlobalKnownHostsFile none\n").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stopped = Arc::new(AtomicBool::new(false));
    let attempts = Arc::new(Mutex::new(Vec::<bool>::new()));
    let observed = attempts.clone();
    let after_stop = stopped.clone();
    let server = tokio::spawn(async move {
        let mut sockets = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            observed.lock().unwrap().push(after_stop.load(Ordering::SeqCst));
            sockets.push(socket); // Hold open without an SSH banner until client timeout.
        }
    });
    let mut config = Config::default();
    config.defaults.retry.connect_timeout_secs = 1;
    for i in 0..5 {
        let mut profile = ServerProfile::new(format!("server-{i}"));
        profile.host = Some("127.0.0.1".into());
        profile.port = Some(port);
        profile.ssh_config = Some(ssh_config.clone());
        profile.known_hosts = Some(directory.path().join("known_hosts"));
        config.forwards.push(ForwardSpec {
            id: format!("rule-{i}"), name: format!("rule-{i}"), group: None,
            server_id: profile.id.clone(), tunnel: Tunnel::Dynamic { listen: ([127,0,0,1],33000+i).into() },
            desired_state: DesiredState::Running, connection_mode: ConnectionMode::Shared,
            remote_cleanup: RemoteCleanup::Off,
        });
        config.servers.push(profile);
    }
    let mut engine = Engine::new();
    engine.reconcile(&config).await.unwrap();
    for _ in 0..100 {
        if attempts.lock().unwrap().len() == 4 { break; }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(attempts.lock().unwrap().len(),4);
    for rule in &mut config.forwards { rule.desired_state = DesiredState::Stopped; }
    engine.reconcile(&config).await.unwrap();
    stopped.store(true,Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1400)).await;
    let recorded = attempts.lock().unwrap().clone();
    assert!(recorded.iter().any(|after| *after));
    println!("{}", serde_json::json!({"connections_before_down":4,"tcp_accepts_after_down_flags":recorded,"status_after_down":engine.snapshot().await}));
    engine.shutdown().await;
    server.abort();
}
