//! Reverse SOCKS requests travel over real SSH forwarded-tcpip channels, then
//! connect to real local TCP targets without a second proxy or listener.
use super::*;
use crate::{engine::forward, model::Tunnel};
use tokio::{io::AsyncWriteExt, net::TcpListener};

type RemoteStream = russh::ChannelStream<server::Msg>;

async fn socks_request(stream: &mut RemoteStream, host: &str, port: u16) -> u8 {
    let mut request = vec![5, 1, 0, 5, 1, 0];
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(address)) => {
            request.push(1);
            request.extend_from_slice(&address.octets());
        }
        Ok(std::net::IpAddr::V6(address)) => {
            request.push(4);
            request.extend_from_slice(&address.octets());
        }
        Err(_) => {
            request.extend_from_slice(&[3, host.len() as u8]);
            request.extend_from_slice(host.as_bytes());
        }
    }
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await.unwrap();
    let mut reply = [0; 12];
    tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&reply[..3], &[5, 0, 5]);
    reply[3]
}

async fn assert_closed(stream: &mut RemoteStream) {
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), stream.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

async fn assert_idle(route: &RemoteRoute) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let active = route.rule.statuses.lock().unwrap()["rule"]
                .status
                .active_connections;
            if active == 0 && route.limit.available_permits() == 1 {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn remote_dynamic_worker_connects_ip_and_local_dns_targets_and_cancels_listener() {
    let behaviour = Behaviour::default();
    let fixture = Fixture::with_behaviour(behaviour.clone()).await;
    let mut rule = test_rule();
    rule.spec.tunnel = Tunnel::RemoteDynamic {
        listen: "127.0.0.1:45000".parse().unwrap(),
    };
    let cancel = CancellationToken::new();
    let worker = tokio::spawn(forward::run(
        rule.clone(),
        fixture.handle.clone(),
        fixture.routes.clone(),
        RetryPolicy::default(),
        cancel.clone(),
        SessionControl::new(None).0,
    ));
    failure_tests::wait_state(&rule, RuntimeState::Established).await;
    assert!(
        fixture
            .routes
            .lock()
            .unwrap()
            .values()
            .all(|route| route.target.is_none())
    );

    for (host, listen) in [
        ("127.0.0.1", "127.0.0.1:0"),
        ("localhost", "127.0.0.1:0"),
        ("::1", "[::1]:0"),
    ] {
        let listener = TcpListener::bind(listen).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let target = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            socket.read_to_end(&mut request).await.unwrap();
            assert_eq!(request, b"request through remote SOCKS");
            socket
                .write_all(b"response after half-close")
                .await
                .unwrap();
        });
        let mut stream = fixture.remote().await.unwrap().into_stream();
        assert_eq!(socks_request(&mut stream, host, port).await, 0);
        assert_eq!(
            rule.statuses.lock().unwrap()["rule"]
                .status
                .active_connections,
            1
        );
        stream
            .write_all(b"request through remote SOCKS")
            .await
            .unwrap();
        stream.shutdown().await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response, b"response after half-close");
        target.await.unwrap();
    }
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .unwrap()
        .unwrap();
    assert!(fixture.routes.lock().unwrap().is_empty());
    assert_eq!(
        behaviour.listens.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        behaviour.cancels.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert!(!fixture.handle.is_closed());
}

#[tokio::test]
async fn remote_dynamic_target_refusal_replies_and_keeps_listener_available() {
    let fixture = Fixture::new().await;
    let route = fixture.remote_route(None, 1);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut stream = fixture.remote().await.unwrap().into_stream();
    assert_eq!(socks_request(&mut stream, "127.0.0.1", port).await, 5);
    assert_closed(&mut stream).await;
    assert_idle(&route).await;
    assert!(!fixture.handle.is_closed());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut stream = fixture.remote().await.unwrap().into_stream();
    assert_eq!(
        socks_request(
            &mut stream,
            "127.0.0.1",
            listener.local_addr().unwrap().port()
        )
        .await,
        0
    );
    let (_target, _) = listener.accept().await.unwrap();
    route.cancel.cancel();
    assert_closed(&mut stream).await;
    assert_idle(&route).await;
}

#[tokio::test]
async fn remote_dynamic_handshake_timeout_releases_capacity_without_closing_ssh() {
    let fixture = Fixture::new().await;
    let mut route = fixture.remote_route(None, 1);
    route.timeout = Duration::from_millis(100);
    fixture
        .routes
        .lock()
        .unwrap()
        .insert(("127.0.0.1".into(), 45000), route.clone());
    let mut events = route.rule.events.subscribe();
    let mut stream = fixture.remote().await.unwrap().into_stream();
    stream.write_all(&[5, 1]).await.unwrap();
    assert_closed(&mut stream).await;
    assert_idle(&route).await;
    assert!(
        events
            .recv()
            .await
            .unwrap()
            .message
            .contains("SOCKS5 handshake timed out")
    );
    assert!(!fixture.handle.is_closed());
}

#[tokio::test]
async fn remote_dynamic_capacity_counts_handshakes_and_cancellation_releases_it() {
    let fixture = Fixture::new().await;
    let route = fixture.remote_route(None, 1);
    let mut stream = fixture.remote().await.unwrap().into_stream();
    stream.write_all(&[5, 1, 0]).await.unwrap();
    let mut selected = [0; 2];
    tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut selected))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(selected, [5, 0]);
    assert_eq!(
        route.rule.statuses.lock().unwrap()["rule"]
            .status
            .active_connections,
        1
    );
    assert!(matches!(
        fixture.remote().await,
        Err(russh::Error::ChannelOpenFailure(
            russh::ChannelOpenFailure::ResourceShortage
        ))
    ));
    route.cancel.cancel();
    assert_closed(&mut stream).await;
    assert_idle(&route).await;
    assert!(matches!(
        fixture.remote().await,
        Err(russh::Error::ChannelOpenFailure(
            russh::ChannelOpenFailure::AdministrativelyProhibited
        ))
    ));
    assert!(!fixture.handle.is_closed());
}

#[tokio::test]
async fn remote_dynamic_unsupported_command_replies_without_connecting() {
    let fixture = Fixture::new().await;
    let route = fixture.remote_route(None, 1);
    let mut stream = fixture.remote().await.unwrap().into_stream();
    stream.write_all(&[5, 1, 0, 5, 3, 0, 1]).await.unwrap();
    let mut reply = [0; 12];
    tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&reply[..4], &[5, 0, 5, 7]);
    assert_closed(&mut stream).await;
    assert_idle(&route).await;
    assert!(!fixture.handle.is_closed());
}

#[tokio::test]
async fn remote_dynamic_verified_listener_waits_for_ownership_and_releases_after_cancel() {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let behaviour = Behaviour::default();
    let mut fixture = Fixture::with_behaviour(behaviour.clone()).await;
    let paths = crate::paths::Paths::new(Some(fixture._directory.path().join("recovery"))).unwrap();
    let context = CleanupContext::open(&paths).unwrap();
    let mut rule = test_rule();
    rule.spec.tunnel = Tunnel::RemoteDynamic {
        listen: "127.0.0.1:45000".parse().unwrap(),
    };
    rule.spec.remote_cleanup = RemoteCleanup::Verified;
    let cancel = CancellationToken::new();
    let worker = tokio::spawn(forward::run(
        rule.clone(),
        fixture.handle.clone(),
        fixture.routes.clone(),
        RetryPolicy::default(),
        cancel.clone(),
        SessionControl::new(Some(context)).0,
    ));
    let channel = tokio::time::timeout(Duration::from_secs(2), fixture.session_channels.recv())
        .await
        .unwrap()
        .unwrap();
    let mut helper = BufReader::new(channel.into_stream());
    for operation in ["claim", "confirm", "release"] {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(2), helper.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["op"], operation);
        assert!(fixture.routes.lock().unwrap().is_empty());
        if operation == "confirm" {
            assert_eq!(
                behaviour.listens.load(std::sync::atomic::Ordering::SeqCst),
                1
            );
            assert!(matches!(
                fixture.remote().await,
                Err(russh::Error::ChannelOpenFailure(_))
            ));
        }
        if operation == "release" {
            assert_eq!(
                behaviour.cancels.load(std::sync::atomic::Ordering::SeqCst),
                1
            );
        }
        let response = serde_json::json!({
            "protocol": 1, "ok": true, "op": operation,
            "generation": request["generation"], "session_id": request["session_id"],
            "session_pid": 123, "reclaimed": false
        });
        helper
            .get_mut()
            .write_all(format!("{response}\n").as_bytes())
            .await
            .unwrap();
        if operation == "confirm" {
            failure_tests::wait_state(&rule, RuntimeState::Established).await;
            assert!(
                fixture
                    .routes
                    .lock()
                    .unwrap()
                    .values()
                    .any(|route| route.target.is_none())
            );
            cancel.cancel();
        }
    }
    tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .unwrap()
        .unwrap();
    assert!(!fixture.handle.is_closed());
}
