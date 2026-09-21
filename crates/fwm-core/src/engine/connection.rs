use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use russh::{
    Channel,
    client::{self, ChannelOpenHandle},
};
use tokio::{
    sync::{Semaphore, mpsc, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::{
    forward::{self, RemoteRoutes},
    retry,
    state::{Events, Rule, now_ms},
};
use crate::{
    cleanup::CleanupContext,
    model::{DesiredState, RemoteCleanup, RetryPolicy, RuntimeState, ServerProfile},
    ssh::{self, SshError},
};

pub(super) type SshHandle = Arc<client::Handle<Handler>>;

#[derive(Clone)]
pub(super) struct Failure {
    message: String,
    needs_attention: bool,
}

#[derive(Clone)]
pub(super) struct SessionControl {
    pub disconnected: CancellationToken,
    pub cleanup: Option<CleanupContext>,
    failures: mpsc::UnboundedSender<Failure>,
    blocked: Arc<Mutex<Option<Failure>>>,
}

impl SessionControl {
    pub fn new(cleanup: Option<CleanupContext>) -> (Self, mpsc::UnboundedReceiver<Failure>) {
        let (failures, receiver) = mpsc::unbounded_channel();
        (
            Self {
                disconnected: CancellationToken::new(),
                cleanup,
                failures,
                blocked: Arc::new(Mutex::new(None)),
            },
            receiver,
        )
    }

    /// Only a verified rule, isolated on its own connection, may request this.
    pub fn reconnect(&self, message: impl Into<String>) {
        let _ = self.failures.send(Failure {
            message: message.into(),
            needs_attention: false,
        });
    }

    pub fn require_attention(&self, message: String) {
        *self.blocked.lock().unwrap() = Some(Failure {
            message,
            needs_attention: true,
        });
    }
}

impl From<SshError> for Failure {
    fn from(error: SshError) -> Self {
        Self {
            needs_attention: error.needs_attention(),
            message: error.to_string(),
        }
    }
}

pub(super) struct Handler {
    resolved: ssh::ResolvedServer,
    routes: RemoteRoutes,
    failure: Arc<Mutex<Option<Failure>>>,
}

impl client::Handler for Handler {
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
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        let route = {
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
        };
        let Some(route) = route else {
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
        tokio::spawn(async move {
            let _permit = permit;
            forward::serve_remote(stream, route).await;
        });
        Ok(())
    }

    async fn disconnected(
        &mut self,
        reason: client::DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        match reason {
            client::DisconnectReason::Error(error) => {
                *self.failure.lock().unwrap() = Some(Failure {
                    message: error.to_string(),
                    needs_attention: error.needs_attention(),
                });
                Err(error)
            }
            client::DisconnectReason::ReceivedDisconnect(info) => {
                *self.failure.lock().unwrap() = Some(Failure {
                    message: format!("server disconnected: {info:?}"),
                    needs_attention: false,
                });
                Ok(())
            }
        }
    }
}

#[cfg(test)]
#[path = "channel_tests.rs"]
mod channel_tests;

struct Worker {
    generation: u64,
    remote_listen: Option<std::net::SocketAddr>,
    verified_cleanup: bool,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

pub(super) async fn supervise(
    profile: ServerProfile,
    policy: RetryPolicy,
    mut desired: watch::Receiver<Vec<Rule>>,
    cancel: CancellationToken,
    limit: Arc<Semaphore>,
    events: Events,
    cleanup: Option<CleanupContext>,
) {
    let mut failures = 0_u32;
    loop {
        let rules = desired.borrow_and_update().clone();
        for rule in &rules {
            if rule.spec.desired_state == DesiredState::Stopped {
                rule.update(RuntimeState::Stopped, None, 0, None);
            }
        }
        if cancel.is_cancelled() {
            return;
        }
        if !rules
            .iter()
            .any(|rule| rule.spec.desired_state == DesiredState::Running)
        {
            tokio::select! {
                _ = cancel.cancelled() => return,
                result = desired.changed() => if result.is_err() { return; },
            }
            continue;
        }
        if cleanup.is_none()
            && rules.iter().any(|rule| {
                rule.spec.desired_state == DesiredState::Running
                    && rule.spec.remote_cleanup == RemoteCleanup::Verified
            })
        {
            set_running(
                &rules,
                RuntimeState::NeedsAttention,
                Some("verified remote cleanup requires a persistent manager identity; start the engine with a CleanupContext".into()),
                failures,
                None,
            );
            tokio::select! {
                _ = cancel.cancelled() => return,
                result = desired.changed() => if result.is_err() { return; },
            }
            continue;
        }
        set_running(&rules, RuntimeState::Starting, None, failures, None);
        let permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            result = desired.changed() => {
                if result.is_err() { return; }
                continue;
            },
            result = limit.clone().acquire_owned() => match result { Ok(permit) => permit, Err(_) => return },
        };
        let routes = Arc::new(Mutex::new(HashMap::new()));
        let failure = Arc::new(Mutex::new(None));
        let result = async {
            let resolved = ssh::resolve(&profile)?;
            ssh::connect(
                &profile,
                &policy,
                Handler {
                    resolved,
                    routes: routes.clone(),
                    failure: failure.clone(),
                },
            )
            .await
        };
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            result = desired.changed() => {
                if result.is_err() { return; }
                continue;
            },
            result = result => result,
        };
        drop(permit);
        let failure = match result {
            Ok(handle) => {
                let since = Instant::now();
                events.emit_server(&profile, "SSH connection established");
                let outcome = connected(
                    Arc::new(handle),
                    routes,
                    failure,
                    &policy,
                    &mut desired,
                    &cancel,
                    cleanup.clone(),
                )
                .await;
                if cancel.is_cancelled() {
                    return;
                }
                if since.elapsed() >= Duration::from_secs(policy.stable_reset_secs) {
                    failures = 0;
                }
                match outcome {
                    Some(failure) => failure,
                    None => {
                        failures = 0;
                        continue;
                    }
                }
            }
            Err(error) => Failure::from(error),
        };
        let rules = desired.borrow_and_update().clone();
        for rule in &rules {
            if rule.spec.desired_state == DesiredState::Stopped {
                rule.update(RuntimeState::Stopped, None, 0, None);
            }
        }
        if !rules
            .iter()
            .any(|rule| rule.spec.desired_state == DesiredState::Running)
        {
            continue;
        }
        if failure.needs_attention {
            set_running(
                &rules,
                RuntimeState::NeedsAttention,
                Some(failure.message),
                failures,
                None,
            );
            tokio::select! {
                _ = cancel.cancelled() => return,
                result = desired.changed() => if result.is_err() { return; },
            }
        } else {
            failures = failures.saturating_add(1);
            let delay = retry::delay(failures, policy.max_delay_secs);
            set_running(
                &rules,
                RuntimeState::Backoff,
                Some(failure.message),
                failures,
                Some(now_ms() + delay.as_millis() as u64),
            );
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(delay) => {},
                result = desired.changed() => if result.is_err() { return; },
            }
        }
    }
}

async fn connected(
    handle: SshHandle,
    routes: RemoteRoutes,
    failure: Arc<Mutex<Option<Failure>>>,
    policy: &RetryPolicy,
    desired: &mut watch::Receiver<Vec<Rule>>,
    cancel: &CancellationToken,
    cleanup: Option<CleanupContext>,
) -> Option<Failure> {
    let mut workers: HashMap<String, Worker> = HashMap::new();
    let (session, mut worker_failures) = SessionControl::new(cleanup);
    let disconnected = session.disconnected.clone();
    let mut check = tokio::time::interval(Duration::from_millis(100));
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let result = loop {
        if cancel.is_cancelled() {
            break None;
        }
        let rules = desired.borrow_and_update().clone();
        let completed: Vec<_> = workers
            .iter()
            .filter(|(_, worker)| worker.task.is_finished())
            .map(|(id, _)| id.clone())
            .collect();
        let mut retire_verified_session = false;
        for id in completed {
            if let Some(worker) = workers.remove(&id) {
                retire_verified_session |= worker.verified_cleanup;
                let _ = worker.task.await;
            }
        }
        if retire_verified_session {
            // The helper associates one lease with one dedicated SSH session.
            // Edits and explicit retries must claim on a fresh session as well.
            break None;
        }
        for (id, worker) in &workers {
            let wanted = rules.iter().find(|rule| rule.spec.id == *id);
            if wanted.is_none_or(|rule| {
                rule.generation != worker.generation
                    || rule.spec.desired_state == DesiredState::Stopped
            }) {
                worker.cancel.cancel();
                if let Some(rule) = wanted {
                    rule.update(
                        RuntimeState::Stopping,
                        Some("waiting for previous listener to close".into()),
                        0,
                        None,
                    );
                }
            }
        }
        for rule in &rules {
            if workers.contains_key(&rule.spec.id) {
                continue;
            }
            if rule.spec.desired_state == DesiredState::Stopped {
                rule.update(RuntimeState::Stopped, None, 0, None);
                continue;
            }
            let remote_listen = rule
                .spec
                .tunnel
                .is_remote()
                .then(|| rule.spec.tunnel.listen());
            if let Some(listen) = remote_listen
                && workers
                    .values()
                    .filter_map(|worker| worker.remote_listen)
                    .any(|old| crate::model::overlaps(old, listen))
            {
                rule.update(
                    RuntimeState::Stopping,
                    Some("waiting for previous owner of remote port to finish cancellation".into()),
                    0,
                    None,
                );
                continue;
            }
            // Observe retirement immediately, even before this supervisor gets
            // CPU time again. A late old worker must not reserve a newer lease.
            let cancel_rule = cancel.child_token();
            let task = tokio::spawn(forward::run(
                rule.clone(),
                handle.clone(),
                routes.clone(),
                policy.clone(),
                cancel_rule.clone(),
                session.clone(),
            ));
            workers.insert(
                rule.spec.id.clone(),
                Worker {
                    generation: rule.generation,
                    remote_listen,
                    verified_cleanup: rule.spec.remote_cleanup == RemoteCleanup::Verified,
                    cancel: cancel_rule,
                    task,
                },
            );
        }
        if cancel.is_cancelled() {
            break None;
        }
        if handle.is_closed() {
            break Some(
                session
                    .blocked
                    .lock()
                    .unwrap()
                    .clone()
                    .or_else(|| failure.lock().unwrap().clone())
                    .unwrap_or(Failure {
                        message: "SSH transport closed".into(),
                        needs_attention: false,
                    }),
            );
        }
        if workers.is_empty() {
            break None;
        }
        tokio::select! {
            _ = cancel.cancelled() => break None,
            Some(failure) = worker_failures.recv() => break Some(failure),
            _ = check.tick() => {},
            result = desired.changed() => if result.is_err() { break None; },
        }
    };
    routes.lock().unwrap().clear();
    if result.is_some() || handle.is_closed() {
        // A broken transport cannot confirm cancellation. Keep ownership records
        // so the next dedicated session can recover its predecessor safely.
        disconnected.cancel();
    }
    let mut workers: Vec<_> = workers.into_values().collect();
    for worker in &workers {
        worker.cancel.cancel();
    }
    // On an intentional shutdown, allow cancel-tcpip-forward and lease release
    // to finish before closing SSH. Bound the whole group, not each worker.
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        for worker in &mut workers {
            let _ = (&mut worker.task).await;
        }
    })
    .await;
    disconnected.cancel();
    for worker in workers {
        if !worker.task.is_finished() {
            worker.task.abort();
            let _ = worker.task.await;
        }
    }
    let _ = tokio::time::timeout(
        Duration::from_secs(1),
        handle.disconnect(russh::Disconnect::ByApplication, "fwm session ended", "en"),
    )
    .await;
    result
}

fn set_running(
    rules: &[Rule],
    state: RuntimeState,
    error: Option<String>,
    failures: u32,
    next: Option<u64>,
) {
    for rule in rules {
        if rule.spec.desired_state == DesiredState::Running {
            rule.update(state, error.clone(), failures, next);
        }
    }
}
