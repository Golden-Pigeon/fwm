//! Reconciles persistent intent with independent rules on supervised SSH connections.
mod channels;
mod connection;
mod forward;
mod lifecycle;
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
};
use state::{Events, Rule, StatusEntry, Statuses};

struct Group {
    profile: ServerProfile,
    policy: crate::model::RetryPolicy,
    desired: watch::Sender<Vec<Rule>>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

pub struct Engine {
    groups: HashMap<String, Group>,
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
        self.ssh_fingerprints
            .retain(|id, _| config.servers.iter().any(|server| server.id == *id));
        for server in config.servers.iter().filter(|server| {
            config
                .forwards
                .iter()
                .any(|rule| rule.server_id == server.id)
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
            let mut connection_profile = server.clone();
            connection_profile.name.clear(); // Labels never change connection identity.
            let key = serde_json::to_string(&(
                connection_profile,
                &config.defaults.retry,
                dedicated,
                &self.ssh_fingerprints[&server.id].1,
            ))?;
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
            desired
                .entry(key)
                .or_insert_with(|| (server.clone(), vec![]))
                .1
                .push(Rule {
                    spec: spec.clone(),
                    server_name: server.name.clone(),
                    generation,
                    statuses: self.statuses.clone(),
                    events: self.events.clone(),
                });
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
        self.revive_finished_groups();
        Ok(())
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

impl Drop for Engine {
    fn drop(&mut self) {
        for group in self.groups.values() {
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
