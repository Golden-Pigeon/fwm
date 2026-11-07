//! Preserve SSH process failures after exec acknowledgement and after EOF.
use super::*;
use crate::cleanup::RemoteLease;
use failure_tests::{claim_reply, cleanup, lease_spec, wait_state};
use serde_json::Value;

async fn request(channel: &mut Channel<server::Msg>, op: &str) -> Value {
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !bytes.ends_with(b"\n") {
            match channel.wait().await {
                Some(russh::ChannelMsg::Data { data }) => bytes.extend_from_slice(&data),
                None | Some(russh::ChannelMsg::Close) => panic!("helper closed before {op}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["op"], op);
    value
}

#[tokio::test]
async fn confirmed_helper_exit_requires_attention_during_claim_confirm_release_and_idle() {
    for phase in ["claim", "confirm", "release", "idle"] {
        for status in [0, 126, 127] {
            for eof_first in [false, true] {
                let mut fixture = Fixture::new().await;
                let handle = fixture.handle.clone();
                let context = cleanup(&fixture);
                let task = tokio::spawn(async move {
                    let result = RemoteLease::claim(
                        handle,
                        &context,
                        &lease_spec(),
                        Duration::from_millis(300),
                    )
                    .await;
                    if phase == "claim" {
                        return result.err().expect("failed helper claimed a lease");
                    }
                    let mut lease = result.unwrap();
                    match phase {
                        "confirm" => lease.confirm().await.unwrap_err(),
                        "release" => lease.release().await.unwrap_err(),
                        "idle" => lease.wait_closed().await,
                        _ => unreachable!(),
                    }
                });
                let mut channel = fixture.session_channels.recv().await.unwrap();
                let claim = request(&mut channel, "claim").await;
                if phase != "claim" {
                    channel
                        .data_bytes(format!("{}\n", claim_reply(&claim)).into_bytes())
                        .await
                        .unwrap();
                    if phase != "idle" {
                        request(&mut channel, phase).await;
                    }
                }
                channel
                    .extended_data_bytes(1, b"recovery helper: Permission denied".to_vec())
                    .await
                    .unwrap();
                if eof_first {
                    channel.eof().await.unwrap();
                    // OpenSSH can deliver the process result after stdout EOF.
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                channel.exit_status(status).await.unwrap();
                if !eof_first {
                    channel.eof().await.unwrap();
                }
                // Retain diagnostics delivered after the exit-status request.
                channel
                    .extended_data_bytes(1, b"; check execution permissions".to_vec())
                    .await
                    .unwrap();
                channel.close().await.unwrap();
                let error = tokio::time::timeout(Duration::from_secs(2), task)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                    error.needs_attention(),
                    "{phase}, exit {status}, EOF first {eof_first}: {error}"
                );
                let message = error.to_string();
                assert!(message.contains(&format!("status {status}")), "{message}");
                assert!(message.contains("Permission denied"), "{message}");
                assert!(message.contains("check execution permissions"), "{message}");
                assert!(!fixture.handle.is_closed());
            }
        }
    }
}

#[tokio::test]
async fn helper_signal_after_exec_ack_requires_attention() {
    let mut fixture = Fixture::new().await;
    let handle = fixture.handle.clone();
    let context = cleanup(&fixture);
    let task = tokio::spawn(async move {
        RemoteLease::claim(handle, &context, &lease_spec(), Duration::from_millis(300)).await
    });
    let mut channel = fixture.session_channels.recv().await.unwrap();
    request(&mut channel, "claim").await;
    channel.eof().await.unwrap();
    fixture
        .server
        .exit_signal_request(
            channel.id(),
            russh::Sig::ILL,
            false,
            "helper crashed".into(),
            "en".into(),
        )
        .await
        .unwrap();
    channel.close().await.unwrap();
    let error = task
        .await
        .unwrap()
        .err()
        .expect("failed helper claimed a lease");
    assert!(error.needs_attention(), "{error}");
    assert!(error.to_string().contains("ILL"), "{error}");
    assert!(error.to_string().contains("helper crashed"), "{error}");
}

#[tokio::test]
async fn unconfirmed_helper_close_keeps_stderr_and_remains_retryable() {
    let mut fixture = Fixture::new().await;
    let handle = fixture.handle.clone();
    let context = cleanup(&fixture);
    let task = tokio::spawn(async move {
        RemoteLease::claim(handle, &context, &lease_spec(), Duration::from_millis(300)).await
    });
    let mut channel = fixture.session_channels.recv().await.unwrap();
    request(&mut channel, "claim").await;
    channel
        .extended_data_bytes(1, b"remote channel interrupted".to_vec())
        .await
        .unwrap();
    channel.close().await.unwrap();
    let error = task
        .await
        .unwrap()
        .err()
        .expect("interrupted helper claimed a lease");
    assert!(!error.needs_attention(), "{error}");
    assert!(
        error.to_string().contains("remote channel interrupted"),
        "{error}"
    );
}

#[tokio::test]
async fn helper_stderr_is_bounded_after_exec_ack() {
    let mut fixture = Fixture::new().await;
    let handle = fixture.handle.clone();
    let context = cleanup(&fixture);
    let task = tokio::spawn(async move {
        RemoteLease::claim(handle, &context, &lease_spec(), Duration::from_millis(300)).await
    });
    let mut channel = fixture.session_channels.recv().await.unwrap();
    request(&mut channel, "claim").await;
    channel
        .extended_data_bytes(1, vec![b'x'; 32769])
        .await
        .unwrap();
    let error = task
        .await
        .unwrap()
        .err()
        .expect("oversized helper stderr accepted");
    assert!(error.needs_attention(), "{error}");
    assert!(
        error.to_string().contains("stderr exceeds 32 KiB"),
        "{error}"
    );
}

#[tokio::test]
async fn confirmed_helper_exit_without_channel_close_still_requires_attention() {
    let mut fixture = Fixture::new().await;
    let handle = fixture.handle.clone();
    let context = cleanup(&fixture);
    let task = tokio::spawn(async move {
        RemoteLease::claim(handle, &context, &lease_spec(), Duration::from_millis(150)).await
    });
    let mut channel = fixture.session_channels.recv().await.unwrap();
    request(&mut channel, "claim").await;
    channel.exit_status(126).await.unwrap();
    let error = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .err()
        .expect("failed helper claimed a lease");
    assert!(error.needs_attention(), "{error}");
    assert!(error.to_string().contains("status 126"), "{error}");
}

#[tokio::test]
async fn confirmed_helper_failure_stops_the_rule_without_requesting_reconnect() {
    let mut fixture = Fixture::new().await;
    let mut rule = test_rule();
    rule.spec = lease_spec();
    let cancel = CancellationToken::new();
    let (session, mut failures) = SessionControl::new(Some(cleanup(&fixture)));
    let task = tokio::spawn(crate::engine::forward::run(
        rule.clone(),
        fixture.handle.clone(),
        fixture.routes.clone(),
        RetryPolicy::default(),
        cancel.clone(),
        session,
    ));
    let mut channel = fixture.session_channels.recv().await.unwrap();
    request(&mut channel, "claim").await;
    channel.exit_status(126).await.unwrap();
    channel.close().await.unwrap();
    wait_state(&rule, RuntimeState::NeedsAttention).await;
    assert!(failures.try_recv().is_err());
    assert!(fixture.routes.lock().unwrap().is_empty());
    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn eof_without_process_result_has_a_cancellation_safe_idle_deadline() {
    let mut fixture = Fixture::new().await;
    let handle = fixture.handle.clone();
    let context = cleanup(&fixture);
    let task = tokio::spawn(async move {
        RemoteLease::claim(handle, &context, &lease_spec(), Duration::from_millis(120))
            .await
            .unwrap()
    });
    let mut channel = fixture.session_channels.recv().await.unwrap();
    let claim = request(&mut channel, "claim").await;
    channel
        .data_bytes(format!("{}\n", claim_reply(&claim)).into_bytes())
        .await
        .unwrap();
    let mut lease = task.await.unwrap();
    channel.eof().await.unwrap();
    let error = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            // The relay supervisor polls other work and recreates this future.
            if let Ok(error) =
                tokio::time::timeout(Duration::from_millis(15), lease.wait_closed()).await
            {
                break error;
            }
        }
    })
    .await
    .unwrap();
    assert!(!error.needs_attention(), "{error}");
    assert!(error.to_string().contains("helper_disconnected"), "{error}");
}
