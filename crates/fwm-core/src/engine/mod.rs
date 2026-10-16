//! Reconciles persistent intent with independent rules on supervised SSH connections.
mod channels;
mod connection;
mod forward;
mod lifecycle;
mod relay;
mod remote;
mod retry;
mod socks;
mod state;

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use tokio::{
    sync::{Semaphore, broadcast, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::model::{
    Config, ConnectionMode, DesiredState, EngineEvent, ForwardStatus, RuntimeState, ServerProfile,
    Tunnel,
};
use relay::RelayEndpoints;
use state::{Events, Rule, StatusEntry, Statuses};

struct Group {
    profile: ServerProfile,
    policy: crate::model::RetryPolicy,
    desired: watch::Sender<Vec<Rule>>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

/// A relay rule always isolates both endpoints on rule-dedicated sessions, so
/// it is supervised per rule rather than through a shared connection group.
/// `connection_key` covers both endpoints' profiles and resolved SSH routes;
/// unlike shared groups, the rule-id key cannot change, so an identity edit
/// must explicitly retire and respawn the coordinator.
struct RelayGroup {
    endpoints: RelayEndpoints,
    policy: crate::model::RetryPolicy,
    connection_key: String,
    desired: watch::Sender<Rule>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

pub struct Engine {
    groups: HashMap<String, Group>,
    relays: HashMap<String, RelayGroup>,
    retired: Vec<JoinHandle<()>>,
    statuses: Statuses,
    events: Events,
    generation: u64,
    connection_limit: Arc<Semaphore>,
    cleanup: Option<crate::cleanup::CleanupContext>,
    ssh_fingerprints: HashMap<String, (ServerProfile, String)>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Self {
            groups: HashMap::new(),
            relays: HashMap::new(),
            retired: vec![],
            statuses: Arc::new(Mutex::new(HashMap::new())),
            events: Events::new(),
            generation: 0,
            connection_limit: Arc::new(Semaphore::new(4)),
            cleanup: None,
            ssh_fingerprints: HashMap::new(),
        }
    }

    pub fn with_cleanup(context: crate::cleanup::CleanupContext) -> Self {
        let mut engine = Self::new();
        engine.cleanup = Some(context);
        engine
    }

    pub async fn reconcile(&mut self, config: &Config) -> Result<()> {
        config.validate().map_err(|error| anyhow!(error))?;
        let mut desired: HashMap<String, (ServerProfile, Vec<Rule>)> = HashMap::new();
        let mut desired_relays: HashMap<String, (RelayEndpoints, Rule, String)> = HashMap::new();
        self.ssh_fingerprints
            .retain(|id, _| config.servers.iter().any(|server| server.id == *id));
        // Fingerprint every server a rule can reference, on either relay side.
        for server in config.servers.iter().filter(|server| {
            config
                .forwards
                .iter()
                .any(|rule| rule.references_server(&server.id))
        }) {
            let mut profile = server.clone();
            profile.name.clear();
            if self
                .ssh_fingerprints
                .get(&server.id)
                .is_none_or(|(previous, _)| *previous != profile)
            {
                let fingerprint = crate::ssh::resolved_route(server)
                    .map(|route| serde_json::to_string(&route).expect("SSH route is serializable"))
                    .unwrap_or_else(|error| format!("invalid SSH configuration: {error}"));
                self.ssh_fingerprints
                    .insert(server.id.clone(), (profile, fingerprint));
            }
        }
        let mut present = HashSet::new();
        for spec in &config.forwards {
            let server = config
                .servers
                .iter()
                .find(|s| s.id == spec.server_id)
                .context("unknown server")?;
            // Verified recovery may terminate this session remotely. Its connection
            // identity must therefore always isolate the rule, including older configs.
            let dedicated = (spec.connection_mode == ConnectionMode::Dedicated
                || spec.remote_cleanup == crate::model::RemoteCleanup::Verified)
                .then_some(&spec.id);
            let key = match &spec.tunnel {
                Tunnel::Relay {
                    target_server_id, ..
                } => {
                    // Both endpoints are dedicated to this rule; identity covers
                    // each side's connection options and resolved SSH route.
                    let target = config
                        .servers
                        .iter()
                        .find(|s| s.id == *target_server_id)
                        .context("unknown target server")?;
                    let mut source_profile = server.clone();
                    source_profile.name.clear();
                    let mut target_profile = target.clone();
                    target_profile.name.clear();
                    serde_json::to_string(&(
                        "relay",
                        &source_profile,
                        &self.ssh_fingerprints[&server.id].1,
                        &target_profile,
                        &self.ssh_fingerprints[target_server_id].1,
                        &config.defaults.retry,
                        &spec.id,
                    ))?
                }
                _ => {
                    let mut connection_profile = server.clone();
                    connection_profile.name.clear(); // Labels never change connection identity.
                    serde_json::to_string(&(
                        connection_profile,
                        &config.defaults.retry,
                        dedicated,
                        &self.ssh_fingerprints[&server.id].1,
                    ))?
                }
            };
            present.insert(spec.id.clone());
            let generation = {
                let mut statuses = self.statuses.lock().unwrap();
                let existing = statuses.get_mut(&spec.id);
                if existing.as_ref().is_some_and(|entry| {
                    runtime_equal(&entry.spec, spec) && entry.connection_key == key
                }) {
                    let entry = existing.unwrap();
                    entry.spec = spec.clone();
                    entry.status.name = spec.name.clone();
                    entry.status.group = spec.group.clone();
                    entry.status.server = server.name.clone();
                    entry.status.target_server = relay_target_name(config, spec);
                    entry.generation
                } else {
                    self.generation += 1;
                    let stopping = spec.desired_state == DesiredState::Stopped
                        && existing.is_some_and(|entry| {
                            !matches!(
                                entry.status.state,
                                RuntimeState::Stopped
                                    | RuntimeState::NeedsAttention
                                    | RuntimeState::Backoff
                            )
                        });
                    let state = if stopping {
                        RuntimeState::Stopping
                    } else if spec.desired_state == DesiredState::Stopped {
                        RuntimeState::Stopped
                    } else {
                        RuntimeState::Starting
                    };
                    statuses.insert(
                        spec.id.clone(),
                        StatusEntry {
                            generation: self.generation,
                            connection_key: key.clone(),
                            spec: spec.clone(),
                            status: ForwardStatus {
                                id: spec.id.clone(),
                                name: spec.name.clone(),
                                group: spec.group.clone(),
                                server: server.name.clone(),
                                target_server: relay_target_name(config, spec),
                                kind: spec.tunnel.kind().to_string(),
                                listen: spec.tunnel.listen().to_string(),
                                target: spec.tunnel.target().map(ToString::to_string),
                                desired_state: spec.desired_state,
                                state,
                                retry_count: 0,
                                next_retry_unix_ms: None,
                                last_error: None,
                                active_connections: 0,
                            },
                        },
                    );
                    self.generation
                }
            };
            let rule = Rule {
                spec: spec.clone(),
                server_name: server.name.clone(),
                generation,
                statuses: self.statuses.clone(),
                events: self.events.clone(),
            };
            match &spec.tunnel {
                Tunnel::Relay {
                    target_server_id, ..
                } => {
                    let target = config
                        .servers
                        .iter()
                        .find(|s| s.id == *target_server_id)
                        .context("unknown target server")?;
                    desired_relays.insert(
                        spec.id.clone(),
                        (
                            RelayEndpoints {
                                source: server.clone(),
                                target: target.clone(),
                            },
                            rule,
                            key,
                        ),
                    );
                }
                _ => {
                    desired
                        .entry(key)
                        .or_insert_with(|| (server.clone(), vec![]))
                        .1
                        .push(rule);
                }
            }
        }
        self.statuses
            .lock()
            .unwrap()
            .retain(|id, _| present.contains(id));
        let obsolete: Vec<_> = self
            .groups
            .keys()
            .filter(|key| !desired.contains_key(*key))
            .cloned()
            .collect();
        for key in obsolete {
            let group = self.groups.remove(&key).unwrap();
            group.cancel.cancel();
            self.retired.push(group.task);
        }
        self.retired.retain(|task| !task.is_finished());
        for (key, (profile, rules)) in desired {
            if let Some(group) = self.groups.get_mut(&key) {
                group.profile = profile;
                group.desired.send_if_modified(|previous| {
                    let changed = previous.len() != rules.len()
                        || previous.iter().any(|old| {
                            !rules.iter().any(|new| {
                                old.spec.id == new.spec.id && old.generation == new.generation
                            })
                        });
                    // Refresh labels without waking a failed connection or its retry timer.
                    *previous = rules;
                    changed
                });
            } else {
                let group = self.spawn_group(profile, config.defaults.retry.clone(), rules);
                self.groups.insert(key, group);
            }
        }
        self.reconcile_relays(desired_relays, config.defaults.retry.clone());
        self.revive_finished_groups();
        self.revive_finished_relays();
        Ok(())
    }

    /// Spawn or refresh per-rule relay coordinators. Endpoint or SSH-route
    /// identity changes rebuild both dedicated sessions; generation bumps and
    /// label-only edits update the running rule in place.
    fn reconcile_relays(
        &mut self,
        desired: HashMap<String, (RelayEndpoints, Rule, String)>,
        policy: crate::model::RetryPolicy,
    ) {
        let stale: Vec<_> = self
            .relays
            .iter()
            .filter(|(id, group)| {
                desired
                    .get(*id)
                    .is_none_or(|(_, _, key)| *key != group.connection_key)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            let group = self.relays.remove(&id).unwrap();
            group.cancel.cancel();
            self.retired.push(group.task);
        }
        for (id, (endpoints, rule, key)) in desired {
            if let Some(group) = self.relays.get_mut(&id) {
                group.endpoints = endpoints;
                group.desired.send_if_modified(|previous| {
                    let changed =
                        previous.generation != rule.generation || previous.spec != rule.spec;
                    *previous = rule;
                    changed
                });
            } else {
                let (desired, receiver) = watch::channel(rule);
                let cancel = CancellationToken::new();
                let task = tokio::spawn(relay::supervise(
                    endpoints.clone(),
                    policy.clone(),
                    receiver,
                    cancel.clone(),
                    self.connection_limit.clone(),
                    self.events.clone(),
                    self.cleanup.clone(),
                ));
                self.relays.insert(
                    id,
                    RelayGroup {
                        endpoints,
                        policy: policy.clone(),
                        connection_key: key,
                        desired,
                        cancel,
                        task,
                    },
                );
            }
        }
    }

    fn revive_finished_relays(&mut self) {
        let finished: Vec<_> = self
            .relays
            .iter()
            .filter(|(_, group)| group.task.is_finished())
            .map(|(id, _)| id.clone())
            .collect();
        for id in finished {
            let old = self.relays.remove(&id).unwrap();
            old.cancel.cancel();
            let rule = old.desired.borrow().clone();
            let (desired, receiver) = watch::channel(rule);
            let cancel = CancellationToken::new();
            let task = tokio::spawn(relay::supervise(
                old.endpoints.clone(),
                old.policy.clone(),
                receiver,
                cancel.clone(),
                self.connection_limit.clone(),
                self.events.clone(),
                self.cleanup.clone(),
            ));
            self.retired.push(old.task);
            self.relays.insert(
                id,
                RelayGroup {
                    endpoints: old.endpoints,
                    policy: old.policy,
                    connection_key: old.connection_key,
                    desired,
                    cancel,
                    task,
                },
            );
        }
    }

    fn spawn_group(
        &self,
        profile: ServerProfile,
        policy: crate::model::RetryPolicy,
        rules: Vec<Rule>,
    ) -> Group {
        let (desired, receiver) = watch::channel(rules);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(connection::supervise(
            profile.clone(),
            policy.clone(),
            receiver,
            cancel.clone(),
            self.connection_limit.clone(),
            self.events.clone(),
            self.cleanup.clone(),
        ));
        Group {
            profile,
            policy,
            desired,
            cancel,
            task,
        }
    }

    fn revive_finished_groups(&mut self) {
        let finished: Vec<_> = self
            .groups
            .iter()
            .filter(|(_, group)| group.task.is_finished())
            .map(|(key, _)| key.clone())
            .collect();
        for key in finished {
            let old = self.groups.remove(&key).unwrap();
            old.cancel.cancel();
            let rules = old.desired.borrow().clone();
            let replacement = self.spawn_group(old.profile, old.policy, rules);
            self.retired.push(old.task);
            self.groups.insert(key, replacement);
        }
    }

    pub async fn snapshot(&self) -> Vec<ForwardStatus> {
        let mut values: Vec<_> = self
            .statuses
            .lock()
            .unwrap()
            .values()
            .map(|entry| entry.status.clone())
            .collect();
        values.sort_by(|a, b| a.name.cmp(&b.name));
        values
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }

    pub async fn shutdown(&mut self) {
        for (_, group) in self.groups.drain() {
            group.cancel.cancel();
            self.retired.push(group.task);
        }
        for (_, group) in self.relays.drain() {
            group.cancel.cancel();
            self.retired.push(group.task);
        }
        join_cancelled(std::mem::take(&mut self.retired)).await;
        for entry in self.statuses.lock().unwrap().values_mut() {
            entry.status.state = RuntimeState::Stopped;
            entry.status.next_retry_unix_ms = None;
            entry.status.active_connections = 0;
        }
    }
}

/// One shared shutdown deadline, followed by an abort/join resource barrier.
async fn join_cancelled(tasks: Vec<JoinHandle<()>>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    for mut task in tasks {
        if tokio::time::timeout_at(deadline, &mut task).await.is_err() {
            task.abort();
            let _ = task.await;
        }
    }
}

fn runtime_equal(a: &crate::model::ForwardSpec, b: &crate::model::ForwardSpec) -> bool {
    a.id == b.id
        && a.server_id == b.server_id
        && a.tunnel == b.tunnel
        && a.desired_state == b.desired_state
        && a.connection_mode == b.connection_mode
        && a.remote_cleanup == b.remote_cleanup
}

/// Display name of a relay rule's target-side server, if it still resolves.
fn relay_target_name(config: &Config, spec: &crate::model::ForwardSpec) -> Option<String> {
    spec.tunnel
        .target_server_id()
        .and_then(|id| config.server(id))
        .map(|server| server.name.clone())
}

impl Drop for Engine {
    fn drop(&mut self) {
        for group in self.groups.values() {
            group.cancel.cancel();
        }
        for group in self.relays.values() {
            group.cancel.cancel();
        }
    }
}

#[cfg(test)]
mod cleanup_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ForwardSpec, RemoteCleanup, Tunnel};

    fn relay_config(running: bool) -> Config {
        let mut source = ServerProfile::new("source-a");
        source.host = Some("127.0.0.1".into());
        let mut target = ServerProfile::new("target-c");
        target.host = Some("127.0.0.1".into());
        Config {
            servers: vec![source.clone(), target.clone()],
            forwards: vec![ForwardSpec {
                id: "relay-rule".into(),
                name: "a-to-c".into(),
                group: None,
                server_id: source.id.clone(),
                tunnel: Tunnel::Relay {
                    listen: "127.0.0.1:18080".parse().unwrap(),
                    target: "localhost:19000".parse().unwrap(),
                    target_server_id: target.id.clone(),
                },
                desired_state: if running {
                    DesiredState::Running
                } else {
                    DesiredState::Stopped
                },
                connection_mode: ConnectionMode::Dedicated,
                remote_cleanup: RemoteCleanup::Off,
            }],
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn relay_rules_get_their_own_coordinator_and_dual_end_labels() {
        let config = relay_config(true);
        let mut engine = Engine::new();
        engine.reconcile(&config).await.unwrap();
        {
            let statuses = engine.statuses.lock().unwrap();
            let entry = &statuses["relay-rule"];
            assert_eq!(entry.status.kind, "relay");
            assert_eq!(entry.status.server, "source-a");
            assert_eq!(entry.status.target_server.as_deref(), Some("target-c"));
            assert_eq!(entry.status.target.as_deref(), Some("localhost:19000"));
            assert_eq!(entry.status.state, RuntimeState::Starting);
        }
        // Relay rules never join a shared connection group.
        assert!(engine.groups.is_empty());
        assert!(engine.relays.contains_key("relay-rule"));
        engine.shutdown().await;
        assert_eq!(engine.snapshot().await[0].state, RuntimeState::Stopped);
    }

    #[tokio::test]
    async fn relay_metadata_edits_keep_identity_while_endpoint_edits_rebuild() {
        let mut config = relay_config(true);
        let mut engine = Engine::new();
        engine.reconcile(&config).await.unwrap();
        let generation = engine.statuses.lock().unwrap()["relay-rule"].generation;
        config.forwards[0].name = "renamed".into();
        engine.reconcile(&config).await.unwrap();
        assert_eq!(
            engine.statuses.lock().unwrap()["relay-rule"].generation,
            generation
        );
        let snapshot = engine.snapshot().await;
        assert_eq!(snapshot[0].name, "renamed");
        // A target endpoint change re-identifies the coordinator.
        config.forwards[0].tunnel = Tunnel::Relay {
            listen: "127.0.0.1:18080".parse().unwrap(),
            target: "localhost:19001".parse().unwrap(),
            target_server_id: config.servers[0].id.clone(),
        };
        engine.reconcile(&config).await.unwrap();
        assert!(engine.statuses.lock().unwrap()["relay-rule"].generation > generation);
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn stopped_relay_rules_report_stopped_without_spawning_a_coordinator_task() {
        // The task exists but immediately reports Stopped and waits; a fresh
        // engine must not treat the rule as connecting.
        let config = relay_config(false);
        let mut engine = Engine::new();
        engine.reconcile(&config).await.unwrap();
        tokio::task::yield_now().await;
        assert_eq!(engine.snapshot().await[0].state, RuntimeState::Stopped);
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn restart_bumps_the_relay_generation_through_its_watch_channel() {
        let config = relay_config(true);
        let mut engine = Engine::new();
        engine.reconcile(&config).await.unwrap();
        let generation = engine.statuses.lock().unwrap()["relay-rule"].generation;
        let report = engine.restart(&["relay-rule".into()]).await.unwrap();
        assert_eq!(report.affected, ["relay-rule"]);
        assert!(engine.statuses.lock().unwrap()["relay-rule"].generation > generation);
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn metadata_changes_preserve_generation_and_shared_connection_identity() {
        let mut server = ServerProfile::new("before");
        server.host = Some("127.0.0.1".into());
        let forward = ForwardSpec {
            id: "forward".into(),
            name: "before".into(),
            group: None,
            server_id: server.id.clone(),
            tunnel: Tunnel::Dynamic {
                listen: "127.0.0.1:1080".parse().unwrap(),
            },
            desired_state: DesiredState::Stopped,
            connection_mode: ConnectionMode::Shared,
            remote_cleanup: RemoteCleanup::Off,
        };
        let mut config = Config {
            servers: vec![server],
            forwards: vec![forward],
            ..Config::default()
        };
        let mut engine = Engine::new();
        engine.reconcile(&config).await.unwrap();
        let before = engine.statuses.lock().unwrap()["forward"].generation;
        let group_key = engine.groups.keys().next().unwrap().clone();
        config.servers[0].name = "after-server".into();
        config.forwards[0].name = "after-forward".into();
        engine.reconcile(&config).await.unwrap();
        assert_eq!(
            engine.statuses.lock().unwrap()["forward"].generation,
            before
        );
        assert!(engine.groups.contains_key(&group_key));
        let status = engine.snapshot().await.remove(0);
        assert_eq!(status.name, "after-forward");
        assert_eq!(status.server, "after-server");
        config.forwards.clear();
        engine.reconcile(&config).await.unwrap();
        tokio::task::yield_now().await;
        assert!(engine.snapshot().await.is_empty());
        engine.shutdown().await;
    }
}

#[cfg(test)]
mod postfix_tests;
