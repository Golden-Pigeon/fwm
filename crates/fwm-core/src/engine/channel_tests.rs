//! Protocol-level regressions for failure and cancellation channel ownership.
use super::*;
use crate::engine::{channels::open_direct, forward::RemoteRoute, state::test_rule};
use crate::model::Endpoint;
use russh::{
    keys::{Algorithm, PrivateKey},
    server,
};
use tokio::{
    io::AsyncReadExt,
    sync::{Notify, mpsc, oneshot},
};

#[derive(Clone, Copy, Default)]
enum ExecMode {
    #[default]
    Success,
    Reject,
    Exit,
    Close,
    Silent,
}
#[derive(Clone, Copy, Default)]
enum DirectMode {
    #[default]
    Delayed,
    Reject,
    Silent,
}
#[derive(Clone, Default)]
struct Behaviour {
    exec: ExecMode,
    session_gate: Option<Arc<Notify>>,
    direct: DirectMode,
    listen: Arc<Mutex<std::collections::VecDeque<bool>>>,
    listen_gate: Option<Arc<Notify>>,
    cancel: Arc<Mutex<std::collections::VecDeque<bool>>>,
    listens: Arc<std::sync::atomic::AtomicUsize>,
    cancels: Arc<std::sync::atomic::AtomicUsize>,
}
struct TestServer {
    behaviour: Behaviour,
    held_direct: Vec<(Channel<server::Msg>, server::ChannelOpenHandle)>,
    direct_started: Arc<Notify>,
    allow_direct: Arc<Notify>,
    direct_channels: mpsc::UnboundedSender<Channel<server::Msg>>,
    session_channels: mpsc::UnboundedSender<Channel<server::Msg>>,
}

impl server::Handler for TestServer {
    type Error = russh::Error;
    async fn auth_none(&mut self, _: &str) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::Accept)
    }
    async fn channel_open_session(
        &mut self,
        channel: Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if let Some(gate) = &self.behaviour.session_gate {
            let gate = gate.clone();
            let sender = self.session_channels.clone();
            tokio::spawn(async move {
                gate.notified().await;
                reply.accept().await;
                let _ = sender.send(channel);
            });
            return Ok(());
        }
        reply.accept().await;
        let _ = self.session_channels.send(channel);
        Ok(())
    }
    async fn exec_request(
        &mut self,
        channel: russh::ChannelId,
        _: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        match self.behaviour.exec {
            ExecMode::Success => session.channel_success(channel)?,
            ExecMode::Reject => session.channel_failure(channel)?,
            ExecMode::Exit => {
                session.extended_data(channel, 1, b"native helper unavailable".as_slice())?;
                session.exit_status_request(channel, 127)?;
            }
            ExecMode::Close => session.close(channel)?,
            ExecMode::Silent => {}
        }
        Ok(())
    }
    async fn cancel_tcpip_forward(
        &mut self,
        _: &str,
        _: u32,
        _: &mut server::Session,
    ) -> Result<bool, Self::Error> {
        self.behaviour
            .cancels
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self
            .behaviour
            .cancel
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(true))
    }
    async fn tcpip_forward(
        &mut self,
        _: &str,
        _: &mut u32,
        _: &mut server::Session,
    ) -> Result<bool, Self::Error> {
        self.behaviour
            .listens
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(gate) = &self.behaviour.listen_gate {
            gate.notified().await;
        }
        Ok(self
            .behaviour
            .listen
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(true))
    }
    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<server::Msg>,
        _: &str,
        _: u32,
        _: &str,
        _: u32,
        reply: server::ChannelOpenHandle,
        _: &mut server::Session,
    ) -> Result<(), Self::Error> {
        match self.behaviour.direct {
            DirectMode::Reject => {
                reply.reject(russh::ChannelOpenFailure::ConnectFailed).await;
                return Ok(());
            }
            DirectMode::Silent => {
                self.held_direct.push((channel, reply));
                return Ok(());
            }
            DirectMode::Delayed => {}
        }
        let allow = self.allow_direct.clone();
        let sender = self.direct_channels.clone();
        self.direct_started.notify_one();
        tokio::spawn(async move {
            allow.notified().await;
            reply.accept().await;
            let _ = sender.send(channel);
        });
        Ok(())
    }
}

struct Fixture {
    handle: SshHandle,
    server: server::Handle,
    routes: RemoteRoutes,
    direct_started: Arc<Notify>,
    allow_direct: Arc<Notify>,
    direct_channels: mpsc::UnboundedReceiver<Channel<server::Msg>>,
    session_channels: mpsc::UnboundedReceiver<Channel<server::Msg>>,
    _directory: tempfile::TempDir,
    task: JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_behaviour(Behaviour::default()).await
    }
    async fn with_behaviour(behaviour: Behaviour) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let key =
            PrivateKey::random(&mut russh::keys::key::safe_rng(), Algorithm::Ed25519).unwrap();
        let known_hosts = directory.path().join("known_hosts");
        std::fs::write(
            &known_hosts,
            format!("127.0.0.1 {}\n", key.public_key().to_openssh().unwrap()),
        )
        .unwrap();
        let config_path = directory.path().join("config");
        std::fs::write(
            &config_path,
            "Host *\n GlobalKnownHostsFile none\n IdentityAgent none\n",
        )
        .unwrap();
        let mut profile = ServerProfile::new("channel-fixture");
        profile.host = Some("127.0.0.1".into());
        profile.known_hosts = Some(known_hosts);
        profile.ssh_config = Some(config_path);
        let routes = Arc::new(Mutex::new(HashMap::new()));
        let handler = Handler {
            resolved: ssh::resolve(&profile).unwrap(),
            routes: routes.clone(),
            failure: Arc::new(Mutex::new(None)),
        };
        let direct_started = Arc::new(Notify::new());
        let allow_direct = Arc::new(Notify::new());
        let (direct_sender, direct_channels) = mpsc::unbounded_channel();
        let (session_sender, session_channels) = mpsc::unbounded_channel();
        let server_handler = TestServer {
            behaviour,
            held_direct: Vec::new(),
            direct_started: direct_started.clone(),
            allow_direct: allow_direct.clone(),
            direct_channels: direct_sender,
            session_channels: session_sender,
        };
        let (client_stream, server_stream) = tokio::io::duplex(65536);
        let (server_sender, server_receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let config = Arc::new(server::Config {
                keys: vec![key],
                auth_rejection_time: Duration::ZERO,
                ..Default::default()
            });
            let running = server::run_stream(config, server_stream, server_handler)
                .await
                .unwrap();
            let _ = server_sender.send(running.handle());
            let _ = running.await;
        });
        let mut handle =
            client::connect_stream(Arc::new(client::Config::default()), client_stream, handler)
                .await
                .unwrap();
        assert!(
            handle
                .authenticate_none("developer")
                .await
                .unwrap()
                .success()
        );
        let server = server_receiver.await.unwrap();
        Self {
            handle: Arc::new(handle),
            server,
            routes,
            direct_started,
            allow_direct,
            direct_channels,
            session_channels,
            _directory: directory,
            task,
        }
    }

    fn route(&self, target: Endpoint, limit: usize) -> RemoteRoute {
        self.remote_route(Some(target), limit)
    }

    fn remote_route(&self, target: Option<Endpoint>, limit: usize) -> RemoteRoute {
        let route = RemoteRoute {
            rule: test_rule(),
            target,
            cancel: CancellationToken::new(),
            limit: Arc::new(Semaphore::new(limit)),
            timeout: Duration::from_secs(1),
        };
        self.routes
            .lock()
            .unwrap()
            .insert(("127.0.0.1".into(), 45000), route.clone());
        route
    }

    async fn remote(&self) -> Result<Channel<server::Msg>, russh::Error> {
        tokio::time::timeout(
            Duration::from_secs(2),
            self.server
                .channel_open_forwarded_tcpip("127.0.0.1", 45000, "127.0.0.1", 50000),
        )
        .await
        .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn retired_remote_worker_cannot_reserve_a_new_recovery_generation() {
    let fixture = Fixture::new().await;
    let paths = crate::paths::Paths::new(Some(fixture._directory.path().join("recovery"))).unwrap();
    let cleanup = CleanupContext::open(&paths).unwrap();
    let recovery_path = paths.state_dir.join("recovery.json");
    let before = std::fs::read(&recovery_path).unwrap();
    let mut rule = test_rule();
    let listen = "127.0.0.1:45000".parse().unwrap();
    let target: Endpoint = "localhost:22".parse().unwrap();
    rule.spec.tunnel = crate::model::Tunnel::Remote {
        listen,
        target: target.clone(),
    };
    rule.spec.remote_cleanup = RemoteCleanup::Verified;
    let cancel = CancellationToken::new();
    let worker_cancel = cancel.child_token();
    cancel.cancel();
    let (session, _) = SessionControl::new(Some(cleanup));
    tokio::time::timeout(
        Duration::from_secs(1),
        crate::engine::remote::run(
            rule,
            fixture.handle.clone(),
            fixture.routes.clone(),
            listen,
            Some(target),
            RetryPolicy::default(),
            worker_cancel,
            session,
        ),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(recovery_path).unwrap(), before);
    assert!(fixture.routes.lock().unwrap().is_empty());
    assert!(!fixture.handle.is_closed());
}

#[tokio::test]
async fn established_listener_detects_helper_exit_and_requests_only_its_session_to_reconnect() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut fixture = Fixture::new().await;
    let sibling = Fixture::new().await;
    let paths = crate::paths::Paths::new(Some(fixture._directory.path().join("recovery"))).unwrap();
    let cleanup = CleanupContext::open(&paths).unwrap();
    let mut rule = test_rule();
    let listen = "127.0.0.1:45000".parse().unwrap();
    let target: Endpoint = "localhost:22".parse().unwrap();
    rule.spec.tunnel = crate::model::Tunnel::Remote {
        listen,
        target: target.clone(),
    };
    rule.spec.remote_cleanup = RemoteCleanup::Verified;
    let mut events = rule.events.subscribe();
    let (session, mut failures) = SessionControl::new(Some(cleanup));
    let disconnected = session.disconnected.clone();
    let task = tokio::spawn(crate::engine::remote::run(
        rule.clone(),
        fixture.handle.clone(),
        fixture.routes.clone(),
        listen,
        Some(target),
        RetryPolicy::default(),
        CancellationToken::new(),
        session,
    ));
    let channel = tokio::time::timeout(Duration::from_secs(1), fixture.session_channels.recv())
        .await
        .unwrap()
        .unwrap();
    let (kill_helper, helper_exit) = oneshot::channel();
    let helper = tokio::spawn(async move {
        let mut io = BufReader::new(channel.into_stream());
        for operation in ["claim", "confirm"] {
            let mut line = String::new();
            io.read_line(&mut line).await.unwrap();
            let command: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(command["op"], operation);
            let response = serde_json::json!({
                "protocol": 1, "ok": true, "op": operation,
                "generation": command["generation"], "session_id": command["session_id"],
                "session_pid": 123, "reclaimed": false
            });
            io.get_mut()
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
            io.get_mut().flush().await.unwrap();
        }
        let _ = helper_exit.await;
        // Dropping only this exec channel simulates Python dying, with SSH live.
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while !events.recv().await.unwrap().message.contains("Established") {}
    })
    .await
    .unwrap();
    assert!(!fixture.routes.lock().unwrap().is_empty());
    kill_helper.send(()).unwrap();
    helper.await.unwrap();
    let failure = tokio::time::timeout(Duration::from_secs(1), failures.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(failure.message.contains("helper_disconnected"));
    assert!(!failure.needs_attention);
    assert!(fixture.routes.lock().unwrap().is_empty());
    assert!(!fixture.handle.is_closed());
    assert!(!sibling.handle.is_closed());
    assert!(!task.is_finished());
    assert_eq!(
        rule.statuses.lock().unwrap()["rule"].status.state,
        RuntimeState::Backoff
    );
    disconnected.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn refused_remote_target_closes_channel_without_closing_shared_transport() {
    let fixture = Fixture::new().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    fixture.route(
        Endpoint {
            host: "127.0.0.1".into(),
            port,
        },
        1,
    );
    let channel = fixture.remote().await.unwrap();
    let mut stream = channel.into_stream();
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), stream.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(!fixture.handle.is_closed());
}

#[tokio::test]
async fn unowned_stopped_and_over_capacity_remote_channels_are_rejected() {
    let fixture = Fixture::new().await;
    assert!(matches!(
        fixture.remote().await,
        Err(russh::Error::ChannelOpenFailure(_))
    ));
    let target = Endpoint {
        host: "127.0.0.1".into(),
        port: 9,
    };
    let route = fixture.route(target.clone(), 1);
    route.cancel.cancel();
    assert!(matches!(
        fixture.remote().await,
        Err(russh::Error::ChannelOpenFailure(_))
    ));
    fixture.route(target, 0);
    assert!(matches!(
        fixture.remote().await,
        Err(russh::Error::ChannelOpenFailure(
            russh::ChannelOpenFailure::ResourceShortage
        ))
    ));
    assert!(!fixture.handle.is_closed());
}

#[tokio::test]
async fn cancelling_accepted_remote_channel_closes_it_and_releases_capacity() {
    let fixture = Fixture::new().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let route = fixture.route(
        Endpoint {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
        },
        1,
    );
    let mut stream = fixture.remote().await.unwrap().into_stream();
    let (_target, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(route.limit.available_permits(), 0);
    route.cancel.cancel();
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), stream.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert_eq!(route.limit.available_permits(), 1);
    assert!(!fixture.handle.is_closed());
}

#[tokio::test]
async fn cancelled_direct_open_closes_late_channel_and_holds_capacity_until_confirmation() {
    let mut fixture = Fixture::new().await;
    let limit = Arc::new(Semaphore::new(1));
    let permit = limit.clone().acquire_owned().await.unwrap();
    let receiver = open_direct(
        fixture.handle.clone(),
        Endpoint {
            host: "127.0.0.1".into(),
            port: 9,
        },
        "127.0.0.1:10000".parse().unwrap(),
        permit,
    );
    tokio::time::timeout(Duration::from_secs(2), fixture.direct_started.notified())
        .await
        .unwrap();
    drop(receiver);
    assert_eq!(limit.available_permits(), 0);
    fixture.allow_direct.notify_one();
    let channel = tokio::time::timeout(Duration::from_secs(2), fixture.direct_channels.recv())
        .await
        .unwrap()
        .unwrap();
    let mut stream = channel.into_stream();
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), stream.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert_eq!(limit.available_permits(), 1);
    assert!(!fixture.handle.is_closed());
}

#[path = "failure_tests.rs"]
mod failure_tests;

#[path = "remote_dynamic_tests.rs"]
mod remote_dynamic_tests;

fn remote_rule(id: &str, listen: &str) -> Rule {
    let mut rule = test_rule();
    rule.spec.id = id.into();
    rule.spec.name = id.into();
    rule.spec.tunnel = crate::model::Tunnel::Remote {
        listen: listen.parse().unwrap(),
        target: "127.0.0.1:9".parse().unwrap(),
    };
    let mut entry = rule.statuses.lock().unwrap().remove("rule").unwrap();
    entry.spec = rule.spec.clone();
    entry.status.id = id.into();
    rule.statuses.lock().unwrap().insert(id.into(), entry);
    rule
}

#[tokio::test]
async fn remote_route_is_unavailable_until_listener_request_is_confirmed() {
    let gate = Arc::new(Notify::new());
    let behaviour = Behaviour {
        listen_gate: Some(gate.clone()),
        ..Default::default()
    };
    let listens = behaviour.listens.clone();
    let fixture = Fixture::with_behaviour(behaviour).await;
    let rule = remote_rule("pending", "127.0.0.1:45000");
    let cancel = CancellationToken::new();
    let (session, _) = SessionControl::new(None);
    let task = tokio::spawn(crate::engine::remote::run(
        rule.clone(),
        fixture.handle.clone(),
        fixture.routes.clone(),
        rule.spec.tunnel.listen(),
        rule.spec.tunnel.target().cloned(),
        RetryPolicy::default(),
        cancel.clone(),
        session,
    ));
    tokio::time::timeout(Duration::from_secs(1), async {
        while listens.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(fixture.routes.lock().unwrap().is_empty());
    gate.notify_one();
    tokio::time::timeout(Duration::from_secs(1), async {
        while fixture.routes.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    task.await.unwrap();
    assert!(fixture.routes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unknown_remote_address_cannot_reuse_a_unique_ports_route() {
    let fixture = Fixture::new().await;
    fixture.route("127.0.0.1:9".parse().unwrap(), 1);
    let result = fixture
        .server
        .channel_open_forwarded_tcpip("127.0.0.2", 45000, "127.0.0.1", 50000)
        .await;
    assert!(matches!(result, Err(russh::Error::ChannelOpenFailure(_))));
}

#[tokio::test]
async fn equivalent_remote_endpoints_wait_for_old_cancellation_but_valid_dual_stack_runs() {
    for (old_address, next_address, expected_listens) in [
        ("127.0.0.1:45000", "[::ffff:127.0.0.1]:45000", 1),
        ("0.0.0.0:45000", "[::1]:45000", 2),
    ] {
        let behaviour = Behaviour::default();
        behaviour
            .cancel
            .lock()
            .unwrap()
            .extend(std::iter::repeat_n(false, 100));
        let listens = behaviour.listens.clone();
        let fixture = Fixture::with_behaviour(behaviour).await;
        let old = remote_rule("old", old_address);
        let (sender, mut desired) = watch::channel(vec![old.clone()]);
        let cancel = CancellationToken::new();
        let task = tokio::spawn({
            let handle = fixture.handle.clone();
            let routes = fixture.routes.clone();
            let cancel = cancel.clone();
            async move {
                connected(
                    handle,
                    routes,
                    Arc::new(Mutex::new(None)),
                    &RetryPolicy::default(),
                    &mut desired,
                    &cancel,
                    None,
                )
                .await
            }
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while fixture.routes.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut retired = old;
        if expected_listens == 1 {
            retired.spec.desired_state = DesiredState::Stopped;
        }
        sender.send_replace(vec![retired, remote_rule("new", next_address)]);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            listens.load(std::sync::atomic::Ordering::SeqCst),
            expected_listens
        );
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }
}
