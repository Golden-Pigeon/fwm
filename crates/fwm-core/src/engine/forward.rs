use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use russh::{ChannelStream, client};
use tokio::{
    io::copy_bidirectional,
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

use super::{
    channels::open_direct,
    connection::{SessionControl, SshHandle},
    remote, retry, socks,
    state::{Rule, now_ms},
};
use crate::model::{Endpoint, RetryPolicy, RuntimeState, Tunnel};

pub(super) type RemoteRoutes = Arc<Mutex<HashMap<(String, u32), RemoteRoute>>>;

#[derive(Clone)]
pub(super) struct RemoteRoute {
    pub rule: Rule,
    /// No fixed target means SOCKS5 requests select a destination on this client.
    pub target: Option<Endpoint>,
    pub cancel: CancellationToken,
    pub limit: Arc<Semaphore>,
    pub timeout: Duration,
}

pub(super) async fn run(
    rule: Rule,
    handle: SshHandle,
    routes: RemoteRoutes,
    policy: RetryPolicy,
    cancel: CancellationToken,
    session: SessionControl,
) {
    match rule.spec.tunnel.clone() {
        Tunnel::Local { listen, target } => {
            local(rule, handle, listen, Some(target), policy, cancel).await
        }
        Tunnel::Dynamic { listen } => local(rule, handle, listen, None, policy, cancel).await,
        Tunnel::Remote { listen, target } => {
            remote::run(
                rule,
                handle,
                routes,
                listen,
                Some(target),
                policy,
                cancel,
                session,
            )
            .await
        }
        Tunnel::RemoteDynamic { listen } => {
            remote::run(rule, handle, routes, listen, None, policy, cancel, session).await
        }
    }
}

async fn local(
    rule: Rule,
    handle: SshHandle,
    listen: SocketAddr,
    target: Option<Endpoint>,
    policy: RetryPolicy,
    cancel: CancellationToken,
) {
    let mut failures: u32 = 0;
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let result = TcpListener::bind(listen).await;
        let listener = match result {
            Ok(listener) => listener,
            Err(error) => {
                failures = failures.saturating_add(1);
                if !backoff(
                    &rule,
                    &cancel,
                    &policy,
                    failures,
                    format!("cannot bind {listen}: {error}"),
                )
                .await
                {
                    return;
                }
                continue;
            }
        };
        rule.update(RuntimeState::Established, None, 0, None);
        let limit = Arc::new(Semaphore::new(256));
        let mut streams = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    streams.abort_all();
                    while streams.join_next().await.is_some() {}
                    return;
                }
                _ = streams.join_next(), if !streams.is_empty() => {},
                accepted = listener.accept() => {
                    let (stream, originator) = match accepted {
                        Ok(accepted) => accepted,
                        Err(error) => {
                            rule.connection_error(format!("listener accept failed: {error}"));
                            tokio::select! { _ = cancel.cancelled() => return, _ = tokio::time::sleep(Duration::from_millis(100)) => {} }
                            continue;
                        }
                    };
                    let Ok(permit) = limit.clone().try_acquire_owned() else { continue; };
                    let rule = rule.clone();
                    let handle = handle.clone();
                    let target = target.clone();
                    let cancel = cancel.clone();
                    let timeout = Duration::from_secs(policy.connect_timeout_secs);
                    streams.spawn(async move {
                        let _active = rule.connection_opened();
                        tokio::select! {
                            _ = cancel.cancelled() => {},
                            result = serve_local(stream, originator, handle, target, timeout, permit) => {
                                if let Err(error) = result { rule.connection_error(error); }
                            }
                        }
                    });
                }
            }
        }
    }
}

async fn serve_local(
    mut stream: TcpStream,
    originator: SocketAddr,
    handle: SshHandle,
    target: Option<Endpoint>,
    timeout: Duration,
    permit: OwnedSemaphorePermit,
) -> Result<()> {
    let dynamic = target.is_none();
    let target = match target {
        Some(target) => target,
        None => tokio::time::timeout(timeout, socks::handshake(&mut stream))
            .await
            .context("SOCKS5 handshake timed out")??,
    };
    let opened =
        tokio::time::timeout(timeout, open_direct(handle, target, originator, permit)).await;
    let (mut channel, _permit) = match opened {
        Ok(Ok(Ok(channel))) => channel,
        Ok(Ok(Err(error))) => {
            if dynamic {
                let _ = socks::reply(&mut stream, 5).await;
            }
            return Err(error.into());
        }
        Ok(Err(error)) => {
            if dynamic {
                let _ = socks::reply(&mut stream, 4).await;
            }
            return Err(error.into());
        }
        Err(error) => {
            if dynamic {
                let _ = socks::reply(&mut stream, 4).await;
            }
            return Err(error.into());
        }
    };
    if dynamic {
        socks::reply(&mut stream, 0).await?;
    }
    copy_bidirectional(&mut stream, &mut channel).await?;
    Ok(())
}

pub(super) async fn serve_remote(mut channel: ChannelStream<client::Msg>, route: RemoteRoute) {
    let _active = route.rule.connection_opened();
    let result = async {
        let dynamic = route.target.is_none();
        let target = match &route.target {
            Some(target) => target.clone(),
            None => tokio::time::timeout(route.timeout, socks::handshake(&mut channel))
                .await
                .context("SOCKS5 handshake timed out")??,
        };
        // Domain requests resolve here, on the fwm client, just as connections
        // to fixed reverse-forward targets do.
        let connected = tokio::time::timeout(
            route.timeout,
            TcpStream::connect((target.host.as_str(), target.port)),
        )
        .await
        .unwrap_or_else(|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "local target connect timed out",
            ))
        });
        let mut stream = match connected {
            Ok(stream) => stream,
            Err(error) => {
                if dynamic {
                    use std::io::ErrorKind;
                    let status = match error.kind() {
                        ErrorKind::PermissionDenied => 2,
                        ErrorKind::NetworkUnreachable => 3,
                        ErrorKind::HostUnreachable => 4,
                        ErrorKind::ConnectionRefused => 5,
                        ErrorKind::TimedOut => 6,
                        _ => 1,
                    };
                    let _ = tokio::time::timeout(route.timeout, socks::reply(&mut channel, status))
                        .await;
                }
                return Err(error.into());
            }
        };
        if dynamic {
            tokio::time::timeout(route.timeout, socks::reply(&mut channel, 0))
                .await
                .context("SOCKS5 response timed out")??;
        }
        copy_bidirectional(&mut stream, &mut channel).await?;
        Ok::<_, anyhow::Error>(())
    };
    tokio::select! {
        biased;
        _ = route.cancel.cancelled() => {},
        result = result => if let Err(error) = result { route.rule.connection_error(error); },
    }
}

pub(super) async fn backoff(
    rule: &Rule,
    cancel: &CancellationToken,
    policy: &RetryPolicy,
    failures: u32,
    error: String,
) -> bool {
    let delay = retry::delay(failures, policy.max_delay_secs);
    rule.update(
        RuntimeState::Backoff,
        Some(error),
        failures,
        Some(now_ms() + delay.as_millis() as u64),
    );
    tokio::select! { _ = cancel.cancelled() => false, _ = tokio::time::sleep(delay) => true }
}
