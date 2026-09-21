use std::{future::Future, net::SocketAddr, sync::Arc, time::Duration};

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::{
    connection::{SessionControl, SshHandle},
    forward::{RemoteRoute, RemoteRoutes, backoff},
    state::Rule,
};
use crate::{
    cleanup::{CleanupError, RemoteLease},
    model::{Endpoint, RemoteCleanup, RetryPolicy, RuntimeState},
};

const VERIFIED_LISTEN_ATTEMPTS: u32 = 3;

#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
    rule: Rule,
    handle: SshHandle,
    routes: RemoteRoutes,
    listen: SocketAddr,
    target: Option<Endpoint>,
    policy: RetryPolicy,
    cancel: CancellationToken,
    session: SessionControl,
) {
    if cancel.is_cancelled() || session.disconnected.is_cancelled() {
        return;
    }
    let address = listen.ip().to_string();
    let port = u32::from(listen.port());
    let key = (address.clone(), port);
    let timeout = Duration::from_secs(policy.connect_timeout_secs);
    let mut lease = if rule.spec.remote_cleanup == RemoteCleanup::Verified {
        let Some(context) = session.cleanup.as_ref() else {
            attention(
                &rule,
                "verified remote cleanup has no persistent manager identity".into(),
                &cancel,
                &session,
            )
            .await;
            return;
        };
        let claimed = tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            _ = session.disconnected.cancelled() => return,
            result = RemoteLease::claim(handle.clone(), context, &rule.spec, timeout) => result,
        };
        match claimed {
            Ok(lease) => {
                if lease.reclaimed() {
                    rule.emit(
                        format!("verified recovery: reclaimed previous SSH session for remote listener {listen}"),
                    );
                }
                Some(lease)
            }
            Err(error) => {
                cleanup_failure(&rule, error, &cancel, &session).await;
                return;
            }
        }
    } else {
        None
    };
    let traffic = cancel.child_token();
    let mut failures = 0_u32;
    loop {
        if session.disconnected.is_cancelled() {
            return;
        }
        if cancel.is_cancelled() {
            release(&mut lease, &rule, &cancel, &session).await;
            return;
        }
        match protocol_request(
            handle.tcpip_forward(address.clone(), port),
            &rule,
            &cancel,
            &session.disconnected,
            timeout,
            "opening remote listener",
        )
        .await
        {
            None => {
                remove_route(&routes, &key, &rule);
                return;
            }
            Some(Err(error)) => {
                remove_route(&routes, &key, &rule);
                if cancel.is_cancelled() {
                    release(&mut lease, &rule, &cancel, &session).await;
                    return;
                }
                failures = failures.saturating_add(1);
                let message = format!(
                    "remote listener {listen} refused (port in use or forwarding prohibited): {error}"
                );
                if lease.is_some() && failures >= VERIFIED_LISTEN_ATTEMPTS {
                    if release(&mut lease, &rule, &cancel, &session).await {
                        attention(&rule, message, &cancel, &session).await;
                    }
                    return;
                }
                if !backoff(&rule, &cancel, &policy, failures, message).await {
                    release(&mut lease, &rule, &cancel, &session).await;
                    return;
                }
            }
            Some(Ok(_)) => break,
        }
    }

    if !cancel.is_cancelled()
        && let Some(lease) = lease.as_mut()
    {
        let confirmation = tokio::select! {
            biased;
            _ = session.disconnected.cancelled() => {
                traffic.cancel();
                remove_route(&routes, &key, &rule);
                return;
            },
            result = lease.confirm() => result,
        };
        if let Err(error) = confirmation {
            traffic.cancel();
            remove_route(&routes, &key, &rule);
            // A protocol success alone is insufficient: compensate for the
            // listener before reporting failed ownership verification.
            if cancel_listener(&handle, &address, port, &rule, &cancel, &session, timeout).await
                && let Err(release_error) = lease.release().await
            {
                rule.emit(format!("remote cleanup release failed: {release_error}"));
            }
            cleanup_failure(&rule, error, &cancel, &session).await;
            return;
        }
    }

    if !cancel.is_cancelled() && !session.disconnected.is_cancelled() {
        routes.lock().unwrap().insert(
            key.clone(),
            RemoteRoute {
                rule: rule.clone(),
                target: target.clone(),
                cancel: traffic.clone(),
                limit: Arc::new(Semaphore::new(256)),
                timeout,
            },
        );
        rule.update(RuntimeState::Established, None, 0, None);
        if let Some(lease) = lease.as_mut() {
            let failure = tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                _ = session.disconnected.cancelled() => None,
                error = lease.wait_closed() => Some(error),
            };
            if let Some(error) = failure {
                traffic.cancel();
                remove_route(&routes, &key, &rule);
                // The control channel can no longer confirm a release. Keep
                // its record and let the next dedicated session recover it.
                cleanup_failure(&rule, error, &cancel, &session).await;
                return;
            }
        } else {
            tokio::select! {
                _ = cancel.cancelled() => {},
                _ = session.disconnected.cancelled() => {},
            }
        }
    }
    traffic.cancel();
    remove_route(&routes, &key, &rule);
    if session.disconnected.is_cancelled() {
        return;
    }
    rule.update(RuntimeState::Stopping, None, 0, None);
    if cancel_listener(&handle, &address, port, &rule, &cancel, &session, timeout).await {
        release(&mut lease, &rule, &cancel, &session).await;
    }
}

async fn release(
    lease: &mut Option<RemoteLease>,
    rule: &Rule,
    cancel: &CancellationToken,
    session: &SessionControl,
) -> bool {
    if session.disconnected.is_cancelled() {
        return false;
    }
    if let Some(lease) = lease.as_mut()
        && let Err(error) = lease.release().await
    {
        cleanup_failure(rule, error, cancel, session).await;
        return false;
    }
    true
}

async fn cleanup_failure(
    rule: &Rule,
    error: CleanupError,
    cancel: &CancellationToken,
    session: &SessionControl,
) {
    if session.disconnected.is_cancelled() {
        return;
    }
    if error.needs_attention() {
        attention(rule, error.to_string(), cancel, session).await;
    } else {
        // A claim is attempted only once per SSH session. A lost helper reply
        // must be recovered on a new dedicated session, retaining its record.
        let message = format!("remote cleanup interrupted: {error}");
        rule.update(RuntimeState::Backoff, Some(message.clone()), 1, None);
        session.reconnect(message);
        tokio::select! {
            _ = cancel.cancelled() => {},
            _ = session.disconnected.cancelled() => {},
        }
    }
}

async fn attention(
    rule: &Rule,
    message: String,
    cancel: &CancellationToken,
    session: &SessionControl,
) {
    session.require_attention(message.clone());
    rule.update(RuntimeState::NeedsAttention, Some(message), 0, None);
    tokio::select! {
        _ = cancel.cancelled() => {},
        _ = session.disconnected.cancelled() => {},
    }
}

#[allow(clippy::too_many_arguments)]
async fn cancel_listener(
    handle: &SshHandle,
    address: &str,
    port: u32,
    rule: &Rule,
    cancel: &CancellationToken,
    session: &SessionControl,
    timeout: Duration,
) -> bool {
    // Retain every global request until its reply arrives or SSH is closed.
    loop {
        match protocol_request(
            handle.cancel_tcpip_forward(address.to_owned(), port),
            rule,
            cancel,
            &session.disconnected,
            timeout,
            "cancelling remote listener",
        )
        .await
        {
            None => return false,
            Some(Ok(())) => return true,
            Some(Err(error)) => {
                rule.update(
                    RuntimeState::Stopping,
                    Some(format!("remote cancellation not confirmed: {error}")),
                    0,
                    None,
                );
                tokio::select! {
                    _ = session.disconnected.cancelled() => return false,
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {},
                }
            }
        }
    }
}

fn remove_route(routes: &RemoteRoutes, key: &(String, u32), rule: &Rule) {
    let mut routes = routes.lock().unwrap();
    if routes
        .get(key)
        .is_some_and(|route| route.rule.generation == rule.generation)
    {
        routes.remove(key);
    }
}

/// Timeout changes observability, not ownership: the future is retained for compensation.
async fn protocol_request<F, T>(
    future: F,
    rule: &Rule,
    cancel: &CancellationToken,
    disconnected: &CancellationToken,
    timeout: Duration,
    operation: &str,
) -> Option<std::result::Result<T, russh::Error>>
where
    F: Future<Output = std::result::Result<T, russh::Error>>,
{
    tokio::pin!(future);
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let mut timed_out = false;
    let mut cancellation_seen = false;
    loop {
        tokio::select! {
            biased;
            _ = disconnected.cancelled() => return None,
            result = &mut future => return Some(result),
            _ = cancel.cancelled(), if !cancellation_seen => {
                cancellation_seen = true;
                rule.update(RuntimeState::Stopping, Some(format!("waiting for protocol result: {operation}")), 0, None);
            }
            _ = &mut deadline, if !timed_out => {
                timed_out = true;
                let state = if cancellation_seen { RuntimeState::Stopping } else { RuntimeState::Unverified };
                rule.update(state, Some(format!("protocol response timed out: {operation}; retaining ownership until confirmed")), 0, None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::test_rule;
    use super::*;

    #[tokio::test]
    async fn cleanup_ownership_failure_emits_attention_without_requesting_reconnect() {
        let rule = test_rule();
        let mut events = rule.events.subscribe();
        let cancel = CancellationToken::new();
        let (session, mut failures) = SessionControl::new(None);
        let task = {
            let rule = rule.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                cleanup_failure(
                    &rule,
                    CleanupError::Remote {
                        code: "ownership_mismatch".into(),
                        message: "port belongs to another process".into(),
                    },
                    &cancel,
                    &session,
                )
                .await;
            })
        };
        let event = events.recv().await.unwrap();
        assert_eq!(event.forward_id.as_deref(), Some("rule"));
        assert!(event.message.contains("NeedsAttention"));
        assert!(event.message.contains("ownership_mismatch"));
        assert!(failures.try_recv().is_err());
        assert!(!task.is_finished());
        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn transient_cleanup_failure_requests_fresh_connection_and_waits_for_it_to_close() {
        let rule = test_rule();
        let mut events = rule.events.subscribe();
        let cancel = CancellationToken::new();
        let (session, mut failures) = SessionControl::new(None);
        let disconnected = session.disconnected.clone();
        let task = {
            let rule = rule.clone();
            tokio::spawn(async move {
                cleanup_failure(
                    &rule,
                    CleanupError::Timeout { operation: "claim" },
                    &cancel,
                    &session,
                )
                .await;
            })
        };
        assert!(failures.recv().await.is_some());
        let event = events.recv().await.unwrap();
        assert!(event.message.contains("Backoff"));
        assert!(event.message.contains("claim timed out"));
        assert!(!task.is_finished());
        disconnected.cancel();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_and_stop_preserve_late_remote_reply_for_compensation() {
        let rule = test_rule();
        let cancel = CancellationToken::new();
        let disconnected = CancellationToken::new();
        let (sender, receiver) = tokio::sync::oneshot::channel::<u32>();
        let task = {
            let rule = rule.clone();
            let cancel = cancel.clone();
            let disconnected = disconnected.clone();
            tokio::spawn(async move {
                protocol_request(
                    async { Ok(receiver.await.unwrap()) },
                    &rule,
                    &cancel,
                    &disconnected,
                    Duration::from_secs(10),
                    "opening remote listener",
                )
                .await
            })
        };
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(11)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            rule.statuses.lock().unwrap()["rule"].status.state,
            RuntimeState::Unverified
        );
        assert!(!task.is_finished());
        cancel.cancel();
        tokio::task::yield_now().await;
        assert_eq!(
            rule.statuses.lock().unwrap()["rule"].status.state,
            RuntimeState::Stopping
        );
        assert!(!task.is_finished());
        sender.send(1080).unwrap();
        assert_eq!(task.await.unwrap().unwrap().unwrap(), 1080);
    }

    #[tokio::test]
    async fn disconnected_session_releases_unconfirmed_request() {
        let rule = test_rule();
        let cancel = CancellationToken::new();
        let disconnected = CancellationToken::new();
        disconnected.cancel();
        let response = protocol_request(
            std::future::pending::<Result<(), russh::Error>>(),
            &rule,
            &cancel,
            &disconnected,
            Duration::from_secs(10),
            "opening remote listener",
        )
        .await;
        assert!(response.is_none());
    }
}
