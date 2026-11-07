//! Exercise errors through real russh channels and real loopback listeners.
use super::*;
use crate::{
    cleanup::{CleanupError, RemoteLease},
    engine::{forward, remote},
    model::{RemoteCleanup, RuntimeState, Tunnel},
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub(super) fn lease_spec() -> crate::model::ForwardSpec {
    let mut spec = test_rule().spec;
    spec.tunnel = Tunnel::Remote {
        listen: "127.0.0.1:45000".parse().unwrap(),
        target: "localhost:22".parse().unwrap(),
    };
    spec.remote_cleanup = RemoteCleanup::Verified;
    spec
}
pub(super) fn cleanup(fixture: &Fixture) -> CleanupContext {
    CleanupContext::open(
        &crate::paths::Paths::new(Some(fixture._directory.path().join("recovery"))).unwrap(),
    )
    .unwrap()
}
pub(super) fn claim_reply(request: &Value) -> Value {
    json!({"protocol":1,"op":"claim","ok":true,"session_id":request["session_id"],"generation":request["generation"],"session_pid":123,"reclaimed":true})
}
pub(super) async fn wait_state(rule: &crate::engine::state::Rule, expected: RuntimeState) {
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if rule.statuses.lock().unwrap()["rule"].status.state == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn unmanaged_port_conflict_keeps_retrying_without_requesting_a_listener() {
    let behaviour = Behaviour::default();
    let listens = behaviour.listens.clone();
    let mut fixture = Fixture::with_behaviour(behaviour).await;
    let mut rule = test_rule();
    rule.spec = lease_spec();
    let (session, mut failures) = SessionControl::new(Some(cleanup(&fixture)));
    let cancel = CancellationToken::new();
    let worker = tokio::spawn(forward::run(
        rule.clone(),
        fixture.handle.clone(),
        fixture.routes.clone(),
        RetryPolicy::default(),
        cancel.clone(),
        session.clone(),
    ));
    let channel = fixture.session_channels.recv().await.unwrap();
    let mut io = BufReader::new(channel.into_stream());
    let mut line = String::new();
    io.read_line(&mut line).await.unwrap();
    let request: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(request["op"], "claim");
    let reply = json!({"protocol":1,"op":"claim","ok":false,
        "code":"unmanaged_conflict","message":"port occupied by an unregistered process"});
    io.get_mut()
        .write_all(format!("{reply}\n").as_bytes())
        .await
        .unwrap();
    let failure = tokio::time::timeout(Duration::from_secs(2), failures.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        !failure.needs_attention,
        "external port conflicts should remain retryable"
    );
    assert!(failure.message.contains("unmanaged_conflict"));
    assert!(session.blocked.lock().unwrap().is_none());
    wait_state(&rule, RuntimeState::Backoff).await;
    assert_eq!(listens.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(fixture.routes.lock().unwrap().is_empty());
    assert!(fixture.session_channels.try_recv().is_err());
    session.disconnected.cancel();
    tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn helper_startup_denial_exit_disconnect_and_timeout_are_actionable() {
    for (mode, needle, needs_attention) in [
        (ExecMode::Reject, "exec_denied", true),
        (
            ExecMode::Exit,
            "native recovery helper exited with status 127",
            true,
        ),
        (ExecMode::Close, "helper_disconnected", false),
        (ExecMode::Silent, "helper startup", false),
    ] {
        let fixture = Fixture::with_behaviour(Behaviour {
            exec: mode,
            ..Default::default()
        })
        .await;
        let result = RemoteLease::claim(
            fixture.handle.clone(),
            &cleanup(&fixture),
            &lease_spec(),
            Duration::from_millis(200),
        )
        .await;
        let error = match result {
            Ok(_) => panic!("invalid helper startup accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains(needle), "{error}");
        assert_eq!(error.needs_attention(), needs_attention, "{error}");
        assert!(
            !fixture.handle.is_closed(),
            "a failed channel must not kill SSH transport"
        );
    }
}

#[tokio::test]
async fn helper_startup_disconnect_requests_a_fresh_session_instead_of_attention() {
    let behaviour = Behaviour {
        exec: ExecMode::Close,
        ..Default::default()
    };
    let listens = behaviour.listens.clone();
    let fixture = Fixture::with_behaviour(behaviour).await;
    let mut rule = test_rule();
    rule.spec = lease_spec();
    let (session, mut failures) = SessionControl::new(Some(cleanup(&fixture)));
    let cancel = CancellationToken::new();
    let worker = tokio::spawn(forward::run(
        rule.clone(),
        fixture.handle.clone(),
        fixture.routes.clone(),
        RetryPolicy::default(),
        cancel.clone(),
        session.clone(),
    ));
    let failure = tokio::time::timeout(Duration::from_secs(2), failures.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(!failure.needs_attention);
    assert!(failure.message.contains("helper_disconnected"));
    assert!(session.blocked.lock().unwrap().is_none());
    wait_state(&rule, RuntimeState::Backoff).await;
    assert!(fixture.routes.lock().unwrap().is_empty());
    assert_eq!(listens.load(std::sync::atomic::Ordering::SeqCst), 0);
    session.disconnected.cancel();
    worker.await.unwrap();
}

#[tokio::test]
async fn helper_replies_validate_json_size_protocol_identity_pid_and_error_code() {
    for case in [
        "json",
        "size",
        "protocol",
        "operation",
        "identity",
        "generation",
        "pid",
        "remote",
        "eof",
        "timeout",
    ] {
        let mut fixture = Fixture::new().await;
        let handle = fixture.handle.clone();
        let context = cleanup(&fixture);
        let task = tokio::spawn(async move {
            RemoteLease::claim(handle, &context, &lease_spec(), Duration::from_millis(250)).await
        });
        let channel = fixture.session_channels.recv().await.unwrap();
        let mut io = BufReader::new(channel.into_stream());
        let mut line = String::new();
        io.read_line(&mut line).await.unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        let mut reply = claim_reply(&request);
        match case {
            "json" => {
                io.get_mut().write_all(b"{no json}\n").await.unwrap();
            }
            "size" => {
                io.get_mut().write_all(&vec![b'x'; 32769]).await.unwrap();
            }
            "eof" => {
                io.get_mut().shutdown().await.unwrap();
                drop(io);
            }
            "timeout" => {}
            other => {
                match other {
                    "protocol" => reply["protocol"] = json!(2),
                    "operation" => reply["op"] = json!("release"),
                    "identity" => reply["session_id"] = json!("foreign"),
                    "generation" => reply["generation"] = json!(999),
                    "pid" => reply["session_pid"] = json!(u64::MAX),
                    "remote" => {
                        reply["ok"] = json!(false);
                        reply["code"] = json!("ownership_mismatch");
                        reply["message"] = json!("foreign process");
                    }
                    _ => unreachable!(),
                }
                io.get_mut()
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .unwrap();
            }
        }
        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        let error = match result {
            Ok(_) => panic!("{case}: malformed helper accepted"),
            Err(e) => e,
        };
        match case {
            "timeout" => assert!(matches!(error, CleanupError::Timeout { .. })),
            "remote" => assert!(error.needs_attention()),
            "eof" => {
                assert!(error.to_string().contains("helper_disconnected"));
                assert!(!error.needs_attention(), "{error}");
            }
            _ => assert!(
                matches!(error, CleanupError::Protocol(_)),
                "{case}: {error}"
            ),
        }
    }
}

#[tokio::test]
async fn lease_confirm_release_and_idle_output_follow_the_protocol_contract() {
    let mut fixture = Fixture::new().await;
    let handle = fixture.handle.clone();
    let context = cleanup(&fixture);
    let task = tokio::spawn(async move {
        RemoteLease::claim(handle, &context, &lease_spec(), Duration::from_secs(1))
            .await
            .unwrap()
    });
    let channel = fixture.session_channels.recv().await.unwrap();
    let mut io = BufReader::new(channel.into_stream());
    let mut line = String::new();
    io.read_line(&mut line).await.unwrap();
    let claim: Value = serde_json::from_str(&line).unwrap();
    io.get_mut()
        .write_all(format!("{}\n", claim_reply(&claim)).as_bytes())
        .await
        .unwrap();
    let mut lease = task.await.unwrap();
    assert!(lease.reclaimed());
    assert_eq!(lease.session_pid(), 123);
    let helper = tokio::spawn(async move {
        for op in ["confirm", "release"] {
            line.clear();
            io.read_line(&mut line).await.unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(req["op"], op);
            for field in ["owner_id", "rule_id", "session_id", "generation"] {
                assert_eq!(req[field], claim[field]);
            }
            if op == "confirm" {
                assert_eq!(req["forward_ack"], true);
            }
            io.get_mut()
                .write_all(format!("{}\n", json!({"protocol":1,"op":op,"ok":true})).as_bytes())
                .await
                .unwrap();
        }
        line.clear();
        assert_eq!(
            io.read_line(&mut line).await.unwrap(),
            0,
            "second release must not send another command"
        );
    });
    lease.confirm().await.unwrap();
    lease.release().await.unwrap();
    lease.release().await.unwrap();
    helper.await.unwrap();
}

#[tokio::test]
async fn local_bind_conflict_recovers_after_release_and_stops_while_in_backoff() {
    for cancel_early in [true, false] {
        let fixture = Fixture::new().await;
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = occupied.local_addr().unwrap();
        let mut rule = test_rule();
        rule.spec.tunnel = Tunnel::Local {
            listen: address,
            target: "localhost:22".parse().unwrap(),
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
        wait_state(&rule, RuntimeState::Backoff).await;
        assert!(
            rule.statuses.lock().unwrap()["rule"]
                .status
                .last_error
                .as_ref()
                .unwrap()
                .contains("cannot bind")
        );
        if cancel_early {
            cancel.cancel();
        } else {
            drop(occupied);
            wait_state(&rule, RuntimeState::Established).await;
            cancel.cancel();
        }
        tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap();
        assert!(!fixture.handle.is_closed());
    }
}

#[tokio::test]
async fn socks_channel_refusal_and_timeout_reply_without_killing_the_shared_transport() {
    for (mode, expected) in [(DirectMode::Reject, 5), (DirectMode::Silent, 4)] {
        let fixture = Fixture::with_behaviour(Behaviour {
            direct: mode,
            ..Default::default()
        })
        .await;
        let reserved = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let mut rule = test_rule();
        rule.spec.tunnel = Tunnel::Dynamic { listen: address };
        let cancel = CancellationToken::new();
        let policy = RetryPolicy {
            connect_timeout_secs: 1,
            ..Default::default()
        };
        let worker = tokio::spawn(forward::run(
            rule.clone(),
            fixture.handle.clone(),
            fixture.routes.clone(),
            policy,
            cancel.clone(),
            SessionControl::new(None).0,
        ));
        wait_state(&rule, RuntimeState::Established).await;
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket
            .write_all(&[5, 1, 0, 5, 1, 0, 1, 127, 0, 0, 1, 0, 22])
            .await
            .unwrap();
        let mut reply = [0; 12];
        tokio::time::timeout(Duration::from_secs(3), socket.read_exact(&mut reply))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&reply[..2], &[5, 0]);
        assert_eq!(reply[3], expected);
        assert!(!fixture.handle.is_closed());
        cancel.cancel();
        worker.await.unwrap();
    }
}

#[tokio::test]
async fn refused_remote_listener_retries_and_failed_cancel_keeps_stopping_until_confirmed() {
    let behaviour = Behaviour::default();
    behaviour.listen.lock().unwrap().extend([false, true]);
    behaviour.cancel.lock().unwrap().extend([false, true]);
    let fixture = Fixture::with_behaviour(behaviour.clone()).await;
    let mut rule = test_rule();
    let listen = "127.0.0.1:45000".parse().unwrap();
    let target: Endpoint = "localhost:22".parse().unwrap();
    rule.spec.tunnel = Tunnel::Remote {
        listen,
        target: target.clone(),
    };
    let cancel = CancellationToken::new();
    let (session, _) = SessionControl::new(None);
    let task = tokio::spawn(remote::run(
        rule.clone(),
        fixture.handle.clone(),
        fixture.routes.clone(),
        listen,
        Some(target),
        RetryPolicy::default(),
        cancel.clone(),
        session,
    ));
    wait_state(&rule, RuntimeState::Established).await;
    assert_eq!(
        behaviour.listens.load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    cancel.cancel();
    wait_state(&rule, RuntimeState::Stopping).await;
    tokio::time::timeout(Duration::from_secs(4), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        behaviour.cancels.load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    assert!(fixture.routes.lock().unwrap().is_empty());
    assert!(!fixture.handle.is_closed());
}

#[tokio::test]
async fn rejected_ownership_confirmation_cancels_the_listener_before_attention() {
    for release_ok in [true, false] {
        let behaviour = Behaviour::default();
        let mut fixture = Fixture::with_behaviour(behaviour.clone()).await;
        let mut rule = test_rule();
        rule.spec = lease_spec();
        let listen = rule.spec.tunnel.listen();
        let target = rule.spec.tunnel.target().unwrap().clone();
        let cancel = CancellationToken::new();
        let (session, _) = SessionControl::new(Some(cleanup(&fixture)));
        let task = tokio::spawn(remote::run(
            rule.clone(),
            fixture.handle.clone(),
            fixture.routes.clone(),
            listen,
            Some(target),
            RetryPolicy::default(),
            cancel.clone(),
            session,
        ));
        let channel = fixture.session_channels.recv().await.unwrap();
        let mut io = BufReader::new(channel.into_stream());
        let mut line = String::new();
        for op in ["claim", "confirm", "release"] {
            line.clear();
            tokio::time::timeout(Duration::from_secs(2), io.read_line(&mut line))
                .await
                .unwrap()
                .unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(req["op"], op);
            let reply = match op {
                "claim" => claim_reply(&req),
                "confirm" => {
                    json!({"protocol":1,"op":"confirm","ok":false,"code":"ownership_mismatch","message":"wrong owner"})
                }
                _ => {
                    assert_eq!(
                        behaviour.cancels.load(std::sync::atomic::Ordering::SeqCst),
                        1
                    );
                    json!({"protocol":1,"op":"release","ok":release_ok,"code":"permission_denied"})
                }
            };
            io.get_mut()
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        }
        wait_state(&rule, RuntimeState::NeedsAttention).await;
        assert!(fixture.routes.lock().unwrap().is_empty());
        assert!(!fixture.handle.is_closed());
        cancel.cancel();
        task.await.unwrap();
    }
}

#[tokio::test]
async fn verified_listener_permission_refusal_stops_after_three_attempts_and_releases_lease() {
    let behaviour = Behaviour::default();
    behaviour
        .listen
        .lock()
        .unwrap()
        .extend([false, false, false]);
    let mut fixture = Fixture::with_behaviour(behaviour.clone()).await;
    let mut rule = test_rule();
    rule.spec = lease_spec();
    let listen = rule.spec.tunnel.listen();
    let target = rule.spec.tunnel.target().unwrap().clone();
    let cancel = CancellationToken::new();
    let (session, _) = SessionControl::new(Some(cleanup(&fixture)));
    let worker = tokio::spawn(remote::run(
        rule.clone(),
        fixture.handle.clone(),
        fixture.routes.clone(),
        listen,
        Some(target),
        RetryPolicy::default(),
        cancel.clone(),
        session,
    ));
    let channel = fixture.session_channels.recv().await.unwrap();
    let mut io = BufReader::new(channel.into_stream());
    let mut line = String::new();
    io.read_line(&mut line).await.unwrap();
    let req: Value = serde_json::from_str(&line).unwrap();
    io.get_mut()
        .write_all(format!("{}\n", claim_reply(&req)).as_bytes())
        .await
        .unwrap();
    line.clear();
    tokio::time::timeout(Duration::from_secs(5), io.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    let release: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(release["op"], "release");
    io.get_mut()
        .write_all(b"{\"protocol\":1,\"op\":\"release\",\"ok\":true}\n")
        .await
        .unwrap();
    wait_state(&rule, RuntimeState::NeedsAttention).await;
    assert_eq!(
        behaviour.listens.load(std::sync::atomic::Ordering::SeqCst),
        3
    );
    assert_eq!(
        behaviour.cancels.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(fixture.routes.lock().unwrap().is_empty());
    cancel.cancel();
    worker.await.unwrap();
}

#[tokio::test]
async fn helper_channel_confirmed_after_open_timeout_is_closed_without_leaking() {
    let gate = Arc::new(Notify::new());
    let mut fixture = Fixture::with_behaviour(Behaviour {
        session_gate: Some(gate.clone()),
        ..Default::default()
    })
    .await;
    let result = RemoteLease::claim(
        fixture.handle.clone(),
        &cleanup(&fixture),
        &lease_spec(),
        Duration::from_millis(100),
    )
    .await;
    assert!(matches!(
        result,
        Err(CleanupError::Timeout {
            operation: "session channel open" | "helper startup"
        })
    ));
    gate.notify_one();
    let channel = fixture.session_channels.recv().await.unwrap();
    let mut stream = channel.into_stream();
    let mut data = [0; 1];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), stream.read(&mut data))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(!fixture.handle.is_closed());
}

#[tokio::test]
async fn unexpected_idle_helper_output_is_a_protocol_failure() {
    let mut fixture = Fixture::new().await;
    let context = cleanup(&fixture);
    let handle = fixture.handle.clone();
    let task = tokio::spawn(async move {
        RemoteLease::claim(handle, &context, &lease_spec(), Duration::from_secs(1))
            .await
            .unwrap()
    });
    let channel = fixture.session_channels.recv().await.unwrap();
    let mut io = BufReader::new(channel.into_stream());
    let mut line = String::new();
    io.read_line(&mut line).await.unwrap();
    let req: Value = serde_json::from_str(&line).unwrap();
    io.get_mut()
        .write_all(format!("{}\nunexpected\n", claim_reply(&req)).as_bytes())
        .await
        .unwrap();
    let mut lease = task.await.unwrap();
    assert!(matches!(
        lease.wait_closed().await,
        CleanupError::Protocol(_)
    ));
}

#[tokio::test]
async fn unconfirmed_listener_cancellation_finishes_teardown_within_its_deadline() {
    // A server that never answers cancel-tcpip-forward must not wedge the
    // worker forever: the bounded compensation leaves the lease record in
    // place and lets the next fenced claim recover the leftover listener.
    let behaviour = Behaviour {
        cancel_gate: Some(Arc::new(Notify::new())),
        ..Default::default()
    };
    let cancels = behaviour.cancels.clone();
    let mut fixture = Fixture::with_behaviour(behaviour).await;
    let mut rule = test_rule();
    rule.spec = lease_spec();
    let policy = RetryPolicy {
        connect_timeout_secs: 2,
        ..Default::default()
    };
    let (session, _failures) = SessionControl::new(Some(cleanup(&fixture)));
    let cancel = CancellationToken::new();
    let mut events = rule.events.subscribe();
    let worker = tokio::spawn(forward::run(
        rule.clone(),
        fixture.handle.clone(),
        fixture.routes.clone(),
        policy,
        cancel.clone(),
        session.clone(),
    ));
    let channel = fixture.session_channels.recv().await.unwrap();
    let mut io = BufReader::new(channel.into_stream());
    let mut line = String::new();
    io.read_line(&mut line).await.unwrap();
    let claim: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(claim["op"], "claim");
    io.get_mut()
        .write_all(format!("{}\n", claim_reply(&claim)).as_bytes())
        .await
        .unwrap();
    line.clear();
    io.read_line(&mut line).await.unwrap();
    let confirm: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(confirm["op"], "confirm");
    io.get_mut()
        .write_all(format!("{}\n", json!({"protocol":1,"op":"confirm","ok":true})).as_bytes())
        .await
        .unwrap();
    wait_state(&rule, RuntimeState::Established).await;

    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(8), worker)
        .await
        .expect("teardown must finish even without a cancellation reply")
        .unwrap();
    assert!(cancels.load(std::sync::atomic::Ordering::SeqCst) >= 1);
    let mut saw_unconfirmed = false;
    while let Ok(event) = events.try_recv() {
        if event.message.contains("was not confirmed in time") {
            saw_unconfirmed = true;
        }
    }
    assert!(
        saw_unconfirmed,
        "the unconfirmed cancellation must be reported honestly"
    );
    assert!(fixture.routes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn desynchronized_request_path_is_detected_and_the_session_rebuilt() {
    // Reproduces the upstream pairing defect with its own public API: a
    // keepalive registered without a reply permanently offsets the reply
    // queue. Keepalive replies then keep the transport looking alive while a
    // probe ping can never resolve. The probe must close the session so
    // verified recovery rebuilds the listener on a fresh one.
    let behaviour = Behaviour {
        cancel_gate: Some(Arc::new(Notify::new())),
        ..Default::default()
    };
    let mut fixture = Fixture::with_behaviour(behaviour).await;
    let mut rule = test_rule();
    rule.spec = lease_spec();
    let policy = RetryPolicy {
        connect_timeout_secs: 2,
        ..Default::default()
    };
    let (session, _failures) = SessionControl::new(Some(cleanup(&fixture)));
    let cancel = CancellationToken::new();
    let mut events = rule.events.subscribe();
    let worker = tokio::spawn(forward::run(
        rule.clone(),
        fixture.handle.clone(),
        fixture.routes.clone(),
        policy,
        cancel.clone(),
        session.clone(),
    ));
    let channel = fixture.session_channels.recv().await.unwrap();
    let mut io = BufReader::new(channel.into_stream());
    let mut line = String::new();
    io.read_line(&mut line).await.unwrap();
    let claim: Value = serde_json::from_str(&line).unwrap();
    io.get_mut()
        .write_all(format!("{}\n", claim_reply(&claim)).as_bytes())
        .await
        .unwrap();
    line.clear();
    io.read_line(&mut line).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap()["op"],
        "confirm"
    );
    io.get_mut()
        .write_all(format!("{}\n", json!({"protocol":1,"op":"confirm","ok":true})).as_bytes())
        .await
        .unwrap();
    wait_state(&rule, RuntimeState::Established).await;

    // Block every global-request reply from this point: the server event loop
    // stalls inside the gated handler, leaving the probe request unanswered
    // while the transport itself stays open. The probe must retire the
    // session so a fresh one can reclaim the listener.
    tokio::time::timeout(Duration::from_secs(20), worker)
        .await
        .expect("the probe must retire the wedged session")
        .unwrap();
    let mut saw_probe = false;
    while let Ok(event) = events.try_recv() {
        if event.message.contains("request-path probe timed out") {
            saw_probe = true;
        }
    }
    assert!(
        saw_probe,
        "the wedged request path must be reported honestly"
    );
}

#[tokio::test]
async fn request_path_probe_helper_classifies_definitive_replies() {
    let healthy = Fixture::new().await;
    let wedged = super::super::request_path_wedged(&healthy.handle, Duration::from_secs(2)).await;
    assert!(!wedged, "healthy fixture must answer the probe");

    let behaviour = Behaviour {
        cancel_gate: Some(Arc::new(Notify::new())),
        ..Default::default()
    };
    let gated = Fixture::with_behaviour(behaviour).await;
    let wedged = super::super::request_path_wedged(&gated.handle, Duration::from_secs(2)).await;
    assert!(wedged, "gated replies must wedge the probe");
}
