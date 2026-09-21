use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use tokio::sync::broadcast;

use crate::model::{EngineEvent, EventContext, ForwardSpec, ForwardStatus, RuntimeState};

pub(super) type Statuses = Arc<Mutex<HashMap<String, StatusEntry>>>;

pub(super) struct StatusEntry {
    pub generation: u64,
    pub connection_key: String,
    pub spec: ForwardSpec,
    pub status: ForwardStatus,
}

#[derive(Clone)]
pub(super) struct Events {
    sender: broadcast::Sender<EngineEvent>,
    sequence: Arc<Mutex<u64>>,
}

impl Events {
    pub fn new() -> Self {
        Self {
            sender: broadcast::channel(512).0,
            sequence: Arc::new(Mutex::new(0)),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.sender.subscribe()
    }

    #[cfg(test)]
    pub fn emit(&self, id: Option<String>, message: impl Into<String>) {
        self.emit_scoped(id, None, message);
    }

    pub fn emit_server(&self, profile: &crate::model::ServerProfile, message: impl Into<String>) {
        self.emit_scoped(
            None,
            Some(EventContext {
                server_id: Some(profile.id.clone()),
                server_name: Some(profile.name.clone()),
                ..Default::default()
            }),
            message,
        );
    }

    fn emit_scoped(
        &self,
        id: Option<String>,
        context: Option<EventContext>,
        message: impl Into<String>,
    ) {
        let mut sequence = self.sequence.lock().unwrap();
        *sequence += 1;
        let _ = self.sender.send(EngineEvent {
            server_id: context
                .as_ref()
                .and_then(|context| context.server_id.clone()),
            context,
            sequence: *sequence,
            timestamp_ms: now_ms(),
            forward_id: id,
            message: message.into(),
        });
    }
}

#[derive(Clone)]
pub(super) struct Rule {
    pub spec: ForwardSpec,
    pub server_name: String,
    pub generation: u64,
    pub statuses: Statuses,
    pub events: Events,
}

impl Rule {
    pub fn update(
        &self,
        state: RuntimeState,
        error: Option<String>,
        retries: u32,
        next: Option<u64>,
    ) {
        let changed = {
            let mut statuses = self.statuses.lock().unwrap();
            let Some(entry) = statuses.get_mut(&self.spec.id) else {
                return;
            };
            if entry.generation != self.generation {
                return;
            }
            let changed = entry.status.state != state || entry.status.last_error != error;
            entry.status.state = state;
            entry.status.last_error = error.clone();
            entry.status.retry_count = retries;
            entry.status.next_retry_unix_ms = next;
            changed
        };
        if changed {
            self.emit(match error {
                Some(error) => format!("{state:?}: {error}"),
                None => format!("{state:?}"),
            });
        }
    }

    pub fn emit(&self, message: impl Into<String>) {
        let statuses = self.statuses.lock().unwrap();
        let entry = statuses
            .get(&self.spec.id)
            .filter(|entry| entry.generation == self.generation);
        let spec = entry.map(|entry| &entry.spec).unwrap_or(&self.spec);
        self.events.emit_scoped(
            Some(spec.id.clone()),
            Some(EventContext {
                forward_name: Some(spec.name.clone()),
                server_id: Some(spec.server_id.clone()),
                server_name: Some(
                    entry
                        .map(|entry| &entry.status.server)
                        .unwrap_or(&self.server_name)
                        .clone(),
                ),
                group: spec.group.clone(),
            }),
            message,
        );
    }

    pub fn connection_opened(&self) -> ConnectionGuard {
        if let Some(entry) = self.statuses.lock().unwrap().get_mut(&self.spec.id)
            && entry.generation == self.generation
        {
            entry.status.active_connections += 1;
        }
        ConnectionGuard(self.clone())
    }

    pub fn connection_error(&self, error: impl std::fmt::Display) {
        // A failed target connection must never tear down the shared SSH transport.
        self.emit(format!("target connection failed: {error}"));
    }
}

pub(super) struct ConnectionGuard(Rule);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        if let Some(entry) = self.0.statuses.lock().unwrap().get_mut(&self.0.spec.id)
            && entry.generation == self.0.generation
        {
            entry.status.active_connections = entry.status.active_connections.saturating_sub(1);
        }
    }
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
pub(super) fn test_rule() -> Rule {
    use crate::model::{ConnectionMode, DesiredState, RemoteCleanup, Tunnel};
    let spec = ForwardSpec {
        id: "rule".into(),
        name: "test".into(),
        group: None,
        server_id: "server".into(),
        tunnel: Tunnel::Dynamic {
            listen: "127.0.0.1:1080".parse().unwrap(),
        },
        desired_state: DesiredState::Running,
        connection_mode: ConnectionMode::Shared,
        remote_cleanup: RemoteCleanup::Off,
    };
    let status = ForwardStatus {
        id: spec.id.clone(),
        name: spec.name.clone(),
        group: spec.group.clone(),
        server: "server".into(),
        kind: "dynamic".into(),
        listen: "127.0.0.1:1080".into(),
        target: None,
        desired_state: DesiredState::Running,
        state: RuntimeState::Starting,
        retry_count: 0,
        next_retry_unix_ms: None,
        last_error: None,
        active_connections: 0,
    };
    let statuses = Arc::new(Mutex::new(HashMap::from([(
        spec.id.clone(),
        StatusEntry {
            generation: 1,
            connection_key: "server".into(),
            spec: spec.clone(),
            status,
        },
    )])));
    Rule {
        spec,
        server_name: "server".into(),
        generation: 1,
        statuses,
        events: Events::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_completion_cannot_overwrite_replacement_or_resurrect_deletion() {
        let rule = test_rule();
        let active = rule.connection_opened();
        {
            let mut statuses = rule.statuses.lock().unwrap();
            let entry = statuses.get_mut("rule").unwrap();
            entry.generation = 2;
            entry.status.active_connections = 7;
        }
        rule.update(RuntimeState::Established, None, 0, None);
        drop(active);
        {
            let statuses = rule.statuses.lock().unwrap();
            assert_eq!(statuses["rule"].status.state, RuntimeState::Starting);
            assert_eq!(statuses["rule"].status.active_connections, 7);
        }
        rule.statuses.lock().unwrap().clear();
        rule.update(RuntimeState::Established, None, 0, None);
        assert!(rule.statuses.lock().unwrap().is_empty());
    }

    #[test]
    fn queued_events_keep_original_labels_and_retired_rules_keep_their_own_server() {
        let rule = test_rule();
        let mut events = rule.events.subscribe();
        rule.emit("before edit");
        let before_edit = now_ms();
        {
            let mut statuses = rule.statuses.lock().unwrap();
            let entry = statuses.get_mut("rule").unwrap();
            entry.spec.name = "renamed".into();
            entry.spec.group = Some("new-group".into());
            entry.status.server = "renamed-server".into();
        }
        rule.emit("same generation after metadata edit");
        {
            let mut statuses = rule.statuses.lock().unwrap();
            let entry = statuses.get_mut("rule").unwrap();
            entry.generation += 1;
            entry.spec.server_id = "replacement-server".into();
            entry.status.server = "replacement-name".into();
        }
        rule.connection_error("late completion from previous server");
        let old = events.try_recv().unwrap();
        assert!(old.timestamp_ms <= before_edit);
        let old_context = old.context.unwrap();
        assert_eq!(old_context.forward_name.as_deref(), Some("test"));
        assert_eq!(old_context.server_name.as_deref(), Some("server"));
        assert_eq!(old_context.group, None);
        let renamed = events.try_recv().unwrap().context.unwrap();
        assert_eq!(renamed.forward_name.as_deref(), Some("renamed"));
        assert_eq!(renamed.server_name.as_deref(), Some("renamed-server"));
        assert_eq!(renamed.group.as_deref(), Some("new-group"));
        let retired = events.try_recv().unwrap();
        assert_eq!(retired.server_id.as_deref(), Some("server"));
        assert_eq!(
            retired.context.unwrap().server_name.as_deref(),
            Some("server")
        );
    }

    #[test]
    fn concurrent_publishers_deliver_monotonic_sequence() {
        let events = Events::new();
        let mut receiver = events.subscribe();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let events = events.clone();
                scope.spawn(move || {
                    for _ in 0..32 {
                        events.emit(None, "update");
                    }
                });
            }
        });
        for sequence in 1..=128 {
            assert_eq!(receiver.try_recv().unwrap().sequence, sequence);
        }
    }
}
