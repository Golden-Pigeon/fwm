//! Rule-level coordinator for two-server relay forwards.
//!
//! One rule owns two dedicated SSH sessions — the source endpoint accepts
//! `forwarded-tcpip` callbacks, the target endpoint opens `direct-tcpip`
//! channels — and bridges them inside this manager. The manager machine (M)
//! relays all data; neither remote server needs access to the other.
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Context;
use russh::{Channel, ChannelStream, client};
use tokio::{
    io::copy_bidirectional,
    sync::{Semaphore, watch},
};
use tokio_util::sync::CancellationToken;

use super::{
    channels::open_direct,
    connection::Failure,
    forward::backoff,
    remote::protocol_request,
    retry,
    state::{Events, Rule, now_ms},
};
use crate::{
    cleanup::{CleanupContext, RemoteLease},
    model::{DesiredState, Endpoint, RemoteCleanup, RetryPolicy, RuntimeState, ServerProfile},
    ssh::{self, SshError},
};

const VERIFIED_LISTEN_ATTEMPTS: u32 = 3;

pub(super) type TargetHandle = Arc<client::Handle<TargetHandler>>;
pub(super) type SourceHandle = Arc<client::Handle<SourceHandler>>;

pub(super) type RelayRoutes = Arc<Mutex<HashMap<(String, u32), RelayRoute>>>;

#[derive(Clone)]
pub(super) struct RelayRoute {
    pub rule: Rule,
    pub target: Endpoint,
    pub target_handle: TargetHandle,
    pub cancel: CancellationToken,
    pub limit: Arc<Semaphore>,
    pub timeout: Duration,
}

#[derive(Clone)]
pub(super) struct RelayEndpoints {
    pub source: ServerProfile,
    pub target: ServerProfile,
}

/// Why an established relay stepped down. Drives teardown and the retry policy.
enum Outcome {
    /// Desired state, rule identity, or an explicit restart changed; re-evaluate.
    DesiredChanged,
    /// A recoverable failure; the message explains the failing endpoint/stage.
    Retry(String),
    /// Requires user intervention (trust, authentication, ownership).
    Attention(String),
}

/// Supervise one relay rule: connect both endpoints, establish the source
/// listener, then keep the bridge healthy until the rule is stopped or edited.
pub(super) async fn supervise(
    endpoints: RelayEndpoints,
    policy: RetryPolicy,
    mut desired: watch::Receiver<Rule>,
    cancel: CancellationToken,
    limit: Arc<Semaphore>,
    events: Events,
    cleanup: Option<CleanupContext>,
) {
    let mut failures = 0_u32;
    loop {
        let rule = desired.borrow_and_update().clone();
        if rule.spec.desired_state == DesiredState::Stopped {
            rule.update(RuntimeState::Stopped, None, 0, None);
        }
        if cancel.is_cancelled() {
            return;
        }
        if rule.spec.desired_state == DesiredState::Stopped {
            tokio::select! {
                _ = cancel.cancelled() => return,
                result = desired.changed() => if result.is_err() { return; },
            }
            continue;
        }
        if cleanup.is_none() && rule.spec.remote_cleanup == RemoteCleanup::Verified {
            rule.update(
                RuntimeState::NeedsAttention,
                Some(
                    "verified remote cleanup requires a persistent manager identity; start the engine with a CleanupContext"
                        .into(),
                ),
                0,
                None,
            );
            tokio::select! {
                _ = cancel.cancelled() => return,
                result = desired.changed() => if result.is_err() { return; },
            }
            continue;
        }
        rule.update(RuntimeState::Starting, None, failures, None);
        // Each endpoint's connection attempt holds one global unit. Acquiring
        // both atomically never holds one unit while waiting for the other, so
        // concurrent relay rules cannot deadlock on the shared limit.
        let permits = tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            result = desired.changed() => { if result.is_err() { return; } continue; },
            result = limit.clone().acquire_many_owned(2) => match result {
                Ok(permits) => permits,
                Err(_) => return,
            },
        };
        let routes: RelayRoutes = Arc::new(Mutex::new(HashMap::new()));
        let source_failure = Arc::new(Mutex::new(None));
        let target_failure = Arc::new(Mutex::new(None));
        let connecting = async {
            let source = async {
                let resolved = ssh::resolve(&endpoints.source)?;
                ssh::connect(
                    &endpoints.source,
                    &policy,
                    SourceHandler {
                        resolved,
                        routes: routes.clone(),
                        failure: source_failure.clone(),
                    },
                )
                .await
            };
            let target = async {
                let resolved = ssh::resolve(&endpoints.target)?;
                ssh::connect(
                    &endpoints.target,
                    &policy,
                    TargetHandler {
                        resolved,
                        failure: target_failure.clone(),
                    },
                )
                .await
            };
            tokio::join!(source, target)
        };
        let (source, target) = tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            result = desired.changed() => { if result.is_err() { return; } continue; },
            result = connecting => result,
        };
        // Permits bound connection attempts only; sessions now own themselves.
        drop(permits);
        let outcome = match (source, target) {
            (Ok(source), Ok(target)) => {
                events.emit_server(&endpoints.source, "relay source SSH connection established");
                events.emit_server(&endpoints.target, "relay target SSH connection established");
                let since = Instant::now();
                let outcome = established(
                    &rule,
                    &policy,
                    Arc::new(source),
                    Arc::new(target),
                    routes,
                    &mut desired,
                    &cancel,
                    cleanup.clone(),
                )
                .await;
                if since.elapsed() >= Duration::from_secs(policy.stable_reset_secs) {
                    failures = 0;
                }
                outcome
            }
            (source, target) => connect_outcome(
                &endpoints,
                &source_failure,
                source.err(),
                &target_failure,
                target.err(),
            ),
        };
        if cancel.is_cancelled() {
            return;
        }
        match outcome {
            Outcome::DesiredChanged => {
                failures = 0;
            }
            Outcome::Retry(message) => {
                failures = failures.saturating_add(1);
                if !backoff(&rule, &cancel, &policy, failures, message).await {
                    return;
                }
            }
            Outcome::Attention(message) => {
                rule.update(RuntimeState::NeedsAttention, Some(message), 0, None);
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    result = desired.changed() => if result.is_err() { return; },
                }
            }
        }
    }
}

/// Combine the endpoints' connection outcomes into one rule-level verdict.
fn connect_outcome(
    endpoints: &RelayEndpoints,
    source_cell: &Arc<Mutex<Option<Failure>>>,
    source_error: Option<SshError>,
    target_cell: &Arc<Mutex<Option<Failure>>>,
    target_error: Option<SshError>,
) -> Outcome {
    let source = source_error
        .map(Failure::from)
        .map(|failure| {
            (
                format!(
                    "source server {}: {}",
                    endpoints.source.name, failure.message
                ),
                failure.needs_attention,
            )
        })
        .or_else(|| {
            source_cell.lock().unwrap().clone().map(|failure| {
                (
                    format!(
                        "source server {}: {}",
                        endpoints.source.name, failure.message
                    ),
                    failure.needs_attention,
                )
            })
        });
    let target = target_error
        .map(Failure::from)
        .map(|failure| {
            (
                format!(
                    "target server {}: {}",
                    endpoints.target.name, failure.message
                ),
                failure.needs_attention,
            )
        })
        .or_else(|| {
            target_cell.lock().unwrap().clone().map(|failure| {
                (
                    format!(
                        "target server {}: {}",
                        endpoints.target.name, failure.message
                    ),
                    failure.needs_attention,
                )
            })
        });
    let (message, needs_attention) = match (source, target) {
        (Some(source), Some(target)) => {
            (format!("{}; {}", source.0, target.0), source.1 || target.1)
        }
        (Some((message, needs_attention)), None) | (None, Some((message, needs_attention))) => {
            (message, needs_attention)
        }
        (None, None) => (
            format!(
                "relay connection to {} and {} failed",
                endpoints.source.name, endpoints.target.name
            ),
            false,
        ),
    };
    if needs_attention {
        Outcome::Attention(message)
    } else {
        Outcome::Retry(message)
    }
}

/// The established phase: claim the source lease, open the source listener,
/// install the bridge route, then supervise both transports until one fails or
/// the rule changes. Readiness is reported only after every side is live.
#[allow(clippy::too_many_arguments)]
async fn established(
    rule: &Rule,
    policy: &RetryPolicy,
    source: SourceHandle,
    target: TargetHandle,
    routes: RelayRoutes,
    desired: &mut watch::Receiver<Rule>,
    cancel: &CancellationToken,
    cleanup: Option<CleanupContext>,
) -> Outcome {
    let listen = rule.spec.tunnel.listen();
    let address = listen.ip().to_string();
    let port = u32::from(listen.port());
    let key = (address.clone(), port);
    let timeout = Duration::from_secs(policy.connect_timeout_secs);
    let target_endpoint = rule
        .spec
        .tunnel
        .target()
        .expect("relay tunnel carries a target")
        .clone();
    let verified = rule.spec.remote_cleanup == RemoteCleanup::Verified;
    let source_gone = CancellationToken::new();

    let mut lease = if verified {
        let Some(context) = cleanup.as_ref() else {
            // The supervisor loop rejects this combination before connecting.
            return Outcome::Attention(
                "verified remote cleanup has no persistent manager identity".into(),
            );
        };
        let claimed = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Outcome::DesiredChanged,
            result = RemoteLease::claim(source.clone(), context, &rule.spec, timeout) => result,
        };
        match claimed {
            Ok(lease) => {
                if lease.reclaimed() {
                    rule.emit(format!(
                        "verified recovery: reclaimed previous SSH session for relay listener {listen}"
                    ));
                }
                Some(lease)
            }
            Err(error) => return cleanup_outcome(error),
        }
    } else {
        None
    };

    // Obtain the source listener. Refusals back off on the same sessions; a
    // verified lease gives up after bounded attempts to avoid churning the
    // helper on a genuinely occupied port.
    let mut failures = 0_u32;
    loop {
        if cancel.is_cancelled() {
            traffic_teardown(rule, &routes, &key);
            if let Some(outcome) = release_lease(&mut lease).await {
                return outcome;
            }
            return Outcome::DesiredChanged;
        }
        if source.is_closed() {
            source_gone.cancel();
            traffic_teardown(rule, &routes, &key);
            return transport_lost("source");
        }
        if target.is_closed() {
            traffic_teardown(rule, &routes, &key);
            return transport_lost("target");
        }
        match protocol_request(
            source.tcpip_forward(address.clone(), port),
            rule,
            cancel,
            &source_gone,
            timeout,
            "opening source listener",
        )
        .await
        {
            None => {
                traffic_teardown(rule, &routes, &key);
                return transport_lost("source");
            }
            Some(Err(error)) => {
                failures = failures.saturating_add(1);
                let message = format!(
                    "source listener {listen} refused (port in use or forwarding prohibited): {error}"
                );
                if lease.is_some() && failures >= VERIFIED_LISTEN_ATTEMPTS {
                    traffic_teardown(rule, &routes, &key);
                    return match release_lease(&mut lease).await {
                        Some(outcome) => outcome,
                        None => Outcome::Attention(message),
                    };
                }
                let delay = retry::delay(failures, policy.max_delay_secs);
                rule.update(
                    RuntimeState::Backoff,
                    Some(message),
                    failures,
                    Some(now_ms() + delay.as_millis() as u64),
                );
                let deadline = tokio::time::sleep(delay);
                tokio::pin!(deadline);
                let mut poll = tokio::time::interval(Duration::from_millis(100));
                poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            traffic_teardown(rule, &routes, &key);
                            if let Some(outcome) = release_lease(&mut lease).await {
                                return outcome;
                            }
                            return Outcome::DesiredChanged;
                        }
                        result = desired.changed() => {
                            if result.is_err() {
                                traffic_teardown(rule, &routes, &key);
                                if let Some(outcome) = release_lease(&mut lease).await {
                                    return outcome;
                                }
                                return Outcome::DesiredChanged;
                            }
                            let next = desired.borrow_and_update().clone();
                            if rule_changed(rule, &next) {
                                traffic_teardown(rule, &routes, &key);
                                if let Some(outcome) = release_lease(&mut lease).await {
                                    return outcome;
                                }
                                return Outcome::DesiredChanged;
                            }
                        }
                        _ = poll.tick() => {
                            if source.is_closed() {
                                source_gone.cancel();
                                traffic_teardown(rule, &routes, &key);
                                return transport_lost("source");
                            }
                            if target.is_closed() {
                                traffic_teardown(rule, &routes, &key);
                                return transport_lost("target");
                            }
                        }
                        _ = &mut deadline => break,
                    }
                }
            }
            Some(Ok(_)) => break,
        }
    }

    if verified {
        // The helper protocol is strictly request/response over one stream: an
        // in-flight lease request must never be dropped and retried, or the
        // helper answers twice and the surplus reply surfaces later as
        // unexpected idle output. confirm() already fails on its own when the
        // transport dies, so nothing may race it except cancellation — and a
        // cancelled attempt abandons the lease for the next session to reclaim
        // instead of issuing a follow-up request on the same stream.
        let confirmation = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            result = lease.as_mut().unwrap().confirm() => Some(result),
        };
        if cancel.is_cancelled() {
            traffic_teardown(rule, &routes, &key);
            drop(lease.take());
            return Outcome::DesiredChanged;
        }
        if source.is_closed() {
            traffic_teardown(rule, &routes, &key);
            return transport_lost("source");
        }
        if let Some(Err(error)) = confirmation {
            traffic_teardown(rule, &routes, &key);
            // A protocol success alone is insufficient: compensate for the
            // listener before reporting failed ownership verification.
            if cancel_listener(&source, &address, port, rule, cancel, &source_gone, timeout).await {
                let _ = release_lease(&mut lease).await;
            }
            return cleanup_outcome(error);
        }
    }

    // The route is visible to source callbacks only after both endpoints and
    // ownership are confirmed; earlier callbacks are rejected, never bridged.
    let traffic = CancellationToken::new();
    routes.lock().unwrap().insert(
        key.clone(),
        RelayRoute {
            rule: rule.clone(),
            target: target_endpoint,
            target_handle: target.clone(),
            cancel: traffic.clone(),
            limit: Arc::new(Semaphore::new(256)),
            timeout,
        },
    );
    rule.update(RuntimeState::Established, None, 0, None);

    let mut check = tokio::time::interval(Duration::from_millis(100));
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let outcome = loop {
        if cancel.is_cancelled() {
            break Outcome::DesiredChanged;
        }
        if source.is_closed() {
            source_gone.cancel();
            break transport_lost("source");
        }
        if target.is_closed() {
            break transport_lost("target");
        }
        let lease_closed = async {
            match lease.as_mut() {
                Some(lease) => lease.wait_closed().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break Outcome::DesiredChanged,
            result = desired.changed() => {
                if result.is_err() {
                    break Outcome::DesiredChanged;
                }
                let next = desired.borrow_and_update().clone();
                if rule_changed(rule, &next) {
                    break Outcome::DesiredChanged;
                }
                // Label-only edit: keep the established bridge untouched.
            }
            _ = check.tick() => {},
            error = lease_closed => {
                // The control channel can no longer confirm a release. Keep its
                // record and let the next dedicated session recover it.
                rule.emit(format!("relay remote cleanup interrupted: {error}"));
                break Outcome::Retry(format!("remote cleanup interrupted: {error}"));
            }
        }
    };
    traffic.cancel();
    remove_route(&routes, &key, rule);
    if !source.is_closed() {
        rule.update(RuntimeState::Stopping, None, 0, None);
        if cancel_listener(&source, &address, port, rule, cancel, &source_gone, timeout).await {
            let _ = release_lease(&mut lease).await;
        }
    }
    close_transport(&source).await;
    close_transport(&target).await;
    outcome
}

/// Label-only edits keep a generation; anything else rebuilds the bridge.
fn rule_changed(current: &Rule, next: &Rule) -> bool {
    next.generation != current.generation || next.spec != current.spec
}

fn transport_lost(side: &str) -> Outcome {
    Outcome::Retry(format!("{side} SSH transport closed"))
}

fn traffic_teardown(rule: &Rule, routes: &RelayRoutes, key: &(String, u32)) {
    if let Some(route) = routes.lock().unwrap().get(key) {
        route.cancel.cancel();
        remove_route(routes, key, rule);
    }
}

fn remove_route(routes: &RelayRoutes, key: &(String, u32), rule: &Rule) {
    let mut routes = routes.lock().unwrap();
    if routes
        .get(key)
        .is_some_and(|route| route.rule.generation == rule.generation)
    {
        routes.remove(key);
    }
}

/// Release a verified lease. Returns `Some(outcome)` when the release failed:
/// the ownership record is kept so the next dedicated session can recover it.
async fn release_lease(lease: &mut Option<RemoteLease>) -> Option<Outcome> {
    match lease.as_mut() {
        Some(lease) => lease.release().await.err().map(cleanup_outcome),
        None => None,
    }
}

fn cleanup_outcome(error: crate::cleanup::CleanupError) -> Outcome {
    if error.needs_attention() {
        Outcome::Attention(error.to_string())
    } else {
        Outcome::Retry(format!("remote cleanup interrupted: {error}"))
    }
}

/// Retain every global request until its reply arrives or SSH is closed.
#[allow(clippy::too_many_arguments)]
async fn cancel_listener<H>(
    handle: &Arc<client::Handle<H>>,
    address: &str,
    port: u32,
    rule: &Rule,
    cancel: &CancellationToken,
    source_gone: &CancellationToken,
    timeout: Duration,
) -> bool
where
    H: client::Handler,
{
    loop {
        match protocol_request(
            handle.cancel_tcpip_forward(address.to_owned(), port),
            rule,
            cancel,
            source_gone,
            timeout,
            "cancelling source listener",
        )
        .await
        {
            None => return false,
            Some(Ok(())) => return true,
            Some(Err(error)) => {
                rule.update(
                    RuntimeState::Stopping,
                    Some(format!(
                        "source listener cancellation not confirmed: {error}"
                    )),
                    0,
                    None,
                );
                tokio::select! {
                    _ = source_gone.cancelled() => return false,
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {},
                }
            }
        }
    }
}

async fn close_transport<H>(handle: &Arc<client::Handle<H>>)
where
    H: client::Handler,
{
    let _ = tokio::time::timeout(
        Duration::from_secs(1),
        handle.disconnect(
            russh::Disconnect::ByApplication,
            "fwm relay session ended",
            "en",
        ),
    )
    .await;
}

pub(super) struct TargetHandler {
    resolved: ssh::ResolvedServer,
    failure: Arc<Mutex<Option<Failure>>>,
}

impl client::Handler for TargetHandler {
    type Error = SshError;

    async fn check_server_key(
        &mut self,
        key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        ssh::verify_host_key(&self.resolved, key)
    }

    async fn disconnected(
        &mut self,
        reason: client::DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        record_disconnect(&self.failure, reason)
    }
}

pub(super) struct SourceHandler {
    resolved: ssh::ResolvedServer,
    routes: RelayRoutes,
    failure: Arc<Mutex<Option<Failure>>>,
}

impl SourceHandler {
    fn route(&self, address: &str, port: u32) -> Option<RelayRoute> {
        let routes = self.routes.lock().unwrap();
        routes
            .get(&(address.to_string(), port))
            .cloned()
            .or_else(|| {
                // Accept only equivalent address spellings, never a different
                // listener just because its port happens to be unique.
                let address = address.parse::<std::net::IpAddr>().ok()?.to_canonical();
                routes
                    .iter()
                    .find_map(|((registered, registered_port), route)| {
                        (*registered_port == port
                            && registered.parse::<std::net::IpAddr>().ok()?.to_canonical()
                                == address)
                            .then(|| route.clone())
                    })
            })
    }
}

impl client::Handler for SourceHandler {
    type Error = SshError;

    async fn check_server_key(
        &mut self,
        key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        ssh::verify_host_key(&self.resolved, key)
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<client::Msg>,
        address: &str,
        port: u32,
        originator_address: &str,
        originator_port: u32,
        reply: russh::client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        let Some(route) = self.route(address, port) else {
            reply
                .reject(russh::ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        };
        if route.cancel.is_cancelled() {
            reply
                .reject(russh::ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        }
        let Ok(permit) = route.limit.clone().try_acquire_owned() else {
            reply
                .reject(russh::ChannelOpenFailure::ResourceShortage)
                .await;
            return Ok(());
        };
        reply.accept().await;
        let stream = channel.into_stream();
        // Originator data is SSH metadata only; it never authenticates the peer.
        let originator = originator_address
            .parse::<std::net::IpAddr>()
            .map(|ip| SocketAddr::new(ip, u16::try_from(originator_port).unwrap_or(0)))
            .unwrap_or_else(|_| SocketAddr::new(std::net::IpAddr::from([127, 0, 0, 1]), 0));
        tokio::spawn(async move {
            bridge(stream, route, originator, permit).await;
        });
        Ok(())
    }

    async fn disconnected(
        &mut self,
        reason: client::DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        record_disconnect(&self.failure, reason)
    }
}

fn record_disconnect(
    failure: &Arc<Mutex<Option<Failure>>>,
    reason: client::DisconnectReason<SshError>,
) -> Result<(), SshError> {
    match reason {
        client::DisconnectReason::Error(error) => {
            *failure.lock().unwrap() = Some(Failure {
                message: error.to_string(),
                needs_attention: error.needs_attention(),
            });
            Err(error)
        }
        client::DisconnectReason::ReceivedDisconnect(info) => {
            *failure.lock().unwrap() = Some(Failure {
                message: format!("server disconnected: {info:?}"),
                needs_attention: false,
            });
            Ok(())
        }
    }
}

/// Bridge one business connection: source `forwarded-tcpip` to target
/// `direct-tcpip`, counted once, with the target's DNS resolved remotely.
async fn bridge(
    mut source: ChannelStream<client::Msg>,
    route: RelayRoute,
    originator: SocketAddr,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    let _active = route.rule.connection_opened();
    let result = async {
        let opened = tokio::time::timeout(
            route.timeout,
            open_direct(
                route.target_handle.clone(),
                route.target.clone(),
                originator,
                permit,
            ),
        )
        .await
        .context("target channel open timed out")?
        .context("target channel request was abandoned")?;
        let (mut target, _permit) = opened?;
        copy_bidirectional(&mut source, &mut target).await?;
        Ok::<_, anyhow::Error>(())
    };
    tokio::select! {
        biased;
        _ = route.cancel.cancelled() => {},
        result = result => if let Err(error) = result {
            // A refused or slow target service fails this connection only;
            // healthy SSH sessions and the listener keep serving new ones.
            route
                .rule
                .connection_error(format!("target connect stage: {error}"));
        },
    }
}
