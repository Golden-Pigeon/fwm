use fwm_api::protocol::EventsReply;
use fwm_core::{
    history::{
        HistoryEntry, HistoryLabels, MAX_MESSAGE_BYTES, append_history, labels_from_config,
        read_history,
    },
    model::{Config, EngineEvent, EventContext, ServerProfile, unix_ms},
};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
};

const RETAIN_EVENTS: usize = 2048;
const MAX_CATALOG_LABELS: usize = 8192;

pub struct EventJournal {
    instance: String,
    path: PathBuf,
    sequence: u64,
    events: VecDeque<EngineEvent>,
    labels: HashMap<String, HistoryLabels>,
    label_order: VecDeque<String>,
    server_labels: HashMap<String, HistoryLabels>,
    server_order: VecDeque<String>,
}

impl EventJournal {
    pub fn new(instance: String, path: PathBuf) -> Self {
        let mut journal = Self {
            instance,
            path,
            sequence: 0,
            events: VecDeque::new(),
            labels: HashMap::new(),
            label_order: VecDeque::new(),
            server_labels: HashMap::new(),
            server_order: VecDeque::new(),
        };
        match read_history(&journal.path) {
            Ok(history) => {
                for warning in history.warnings {
                    tracing::warn!(%warning, "event history was only partially readable");
                }
                for entry in history.entries {
                    if let Some(id) = &entry.server_id {
                        journal.remember_server(id.clone(), HistoryLabels::from_entry(&entry));
                    }
                    if entry.forward_name.is_some()
                        && let Some(id) = &entry.event.forward_id
                    {
                        journal.remember(id.clone(), HistoryLabels::from_entry(&entry));
                    }
                }
            }
            Err(error) => tracing::warn!(%error, "cannot restore historical event labels"),
        }
        journal
    }

    /// Keep deleted IDs available for late stop/cleanup events; labels already
    /// persisted on historical events retain their original name and scope.
    pub fn set_config(&mut self, config: &Config) {
        for server in &config.servers {
            self.set_server(server);
        }
        for (id, labels) in labels_from_config(config) {
            self.remember(id, labels);
        }
    }

    pub fn set_server(&mut self, server: &ServerProfile) {
        self.remember_server(
            server.id.clone(),
            HistoryLabels {
                server_id: Some(server.id.clone()),
                server_name: Some(server.name.clone()),
                ..Default::default()
            },
        );
    }

    fn remember_server(&mut self, id: String, mut labels: HistoryLabels) {
        labels.forward_name = None;
        labels.group = None;
        if !self.server_labels.contains_key(&id) {
            self.server_order.push_back(id.clone());
        }
        self.server_labels.insert(id, labels);
        while self.server_labels.len() > MAX_CATALOG_LABELS {
            if let Some(id) = self.server_order.pop_front() {
                self.server_labels.remove(&id);
            }
        }
    }

    fn remember(&mut self, id: String, labels: HistoryLabels) {
        if !self.labels.contains_key(&id) {
            self.label_order.push_back(id.clone());
        }
        self.labels.insert(id, labels);
        while self.labels.len() > MAX_CATALOG_LABELS {
            if let Some(oldest) = self.label_order.pop_front() {
                self.labels.remove(&oldest);
            }
        }
    }
    pub fn record(&mut self, forward_id: Option<String>, message: String) {
        self.record_scoped(forward_id, None, message);
    }

    pub fn record_server(&mut self, server_id: String, message: String) {
        self.record_scoped(None, Some(server_id), message);
    }

    pub fn record_engine(&mut self, event: EngineEvent) {
        // Producers own event time and context; never replace them with labels
        // from a newer configuration when consuming a queued event.
        self.persist(event);
    }

    fn record_scoped(
        &mut self,
        forward_id: Option<String>,
        server_id: Option<String>,
        message: String,
    ) {
        let labels = forward_id
            .as_ref()
            .and_then(|id| self.labels.get(id))
            .filter(|labels| server_id.is_none() || labels.server_id == server_id)
            .or_else(|| server_id.as_ref().and_then(|id| self.server_labels.get(id)))
            .cloned()
            .unwrap_or_default();
        let has_context = labels.forward_name.is_some()
            || labels.server_id.is_some()
            || labels.server_name.is_some()
            || labels.group.is_some()
            || server_id.is_some();
        self.persist(EngineEvent {
            context: has_context.then(|| EventContext {
                forward_name: labels.forward_name,
                server_id: server_id.clone().or(labels.server_id.clone()),
                server_name: labels.server_name,
                group: labels.group,
            }),
            server_id: server_id.or(labels.server_id),
            sequence: 0,
            timestamp_ms: unix_ms(),
            forward_id,
            message,
        });
    }

    fn persist(&mut self, mut event: EngineEvent) {
        if event.message.len() > MAX_MESSAGE_BYTES {
            let mut boundary = MAX_MESSAGE_BYTES;
            while !event.message.is_char_boundary(boundary) {
                boundary -= 1;
            }
            event.message.truncate(boundary);
            event.message.push_str(" [truncated]");
        }
        self.sequence += 1;
        event.sequence = self.sequence;
        if let Some(context) = &event.context {
            event.server_id = context.server_id.clone().or(event.server_id);
        }
        self.events.push_back(event.clone());
        if self.events.len() > RETAIN_EVENTS {
            self.events.pop_front();
        }
        let entry = HistoryEntry::new(self.instance.clone(), event);
        if let Err(error) = append_history(&self.path, &entry) {
            tracing::warn!(%error, "cannot persist event log");
        }
    }
    pub fn since(&self, after: u64) -> EventsReply {
        let oldest = self
            .events
            .front()
            .map_or(self.sequence + 1, |e| e.sequence);
        let mut bytes = 0;
        let events: Vec<_> = self
            .events
            .iter()
            .filter(|e| e.sequence > after || after > self.sequence)
            .take_while(|event| {
                bytes += serde_json::to_vec(event).map_or(8192, |value| value.len());
                bytes <= 256 * 1024
            })
            .cloned()
            .collect();
        let blocked = events.is_empty()
            && self
                .events
                .iter()
                .any(|e| e.sequence > after || after > self.sequence);
        let next_sequence = events.last().map_or(
            if blocked {
                after.min(self.sequence)
            } else {
                self.sequence
            },
            |event| event.sequence,
        );
        EventsReply {
            daemon_instance_id: self.instance.clone(),
            latest_sequence: self.sequence,
            next_sequence,
            has_more: next_sequence < self.sequence,
            resync_required: blocked
                || after > self.sequence
                || (after != 0 && after.saturating_add(1) < oldest),
            events,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fwm_core::model::{
        ConnectionMode, DesiredState, ForwardSpec, RemoteCleanup, ServerProfile, Tunnel,
    };

    #[test]
    fn labels_survive_rename_delete_and_restart_without_relabeling_old_events() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("events.jsonl");
        let mut server = ServerProfile::new("dev");
        server.host = Some("127.0.0.1".into());
        let forward = ForwardSpec {
            id: "rule".into(),
            name: "old-name".into(),
            server_id: server.id.clone(),
            group: Some("batch".into()),
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
            ..Default::default()
        };
        let mut journal = EventJournal::new("one".into(), path.clone());
        journal.set_config(&config);
        journal.record(Some("rule".into()), "before rename".into());
        config.forwards[0].name = "new-name".into();
        journal.set_config(&config);
        journal.record(Some("rule".into()), "after rename".into());
        config.forwards.clear();
        journal.set_config(&config);
        journal.record(Some("rule".into()), "after delete".into());
        drop(journal);
        let mut restarted = EventJournal::new("two".into(), path.clone());
        restarted.set_config(&config);
        restarted.record(Some("rule".into()), "late cleanup after restart".into());
        let history = read_history(&path).unwrap();
        assert_eq!(history.entries.len(), 4);
        assert_eq!(history.entries[0].forward_name.as_deref(), Some("old-name"));
        for entry in &history.entries[1..] {
            assert_eq!(entry.forward_name.as_deref(), Some("new-name"));
            assert_eq!(entry.group.as_deref(), Some("batch"));
            assert_eq!(entry.server_name.as_deref(), Some("dev"));
        }
        assert_eq!(history.entries[3].daemon_instance_id, "two");
        assert_eq!(history.entries[3].event.sequence, 1);
        assert_eq!(
            restarted.since(0).events.len(),
            1,
            "SDK cursor belongs to this daemon instance only"
        );
    }

    #[test]
    fn bounded_log_reports_cursor_gaps() {
        let temp = tempfile::tempdir().unwrap();
        let mut journal = EventJournal::new("instance".into(), temp.path().join("events.jsonl"));
        for i in 0..(RETAIN_EVENTS + 3) {
            journal.record(None, format!("event {i}"));
        }
        assert!(journal.since(1).resync_required);
        assert_eq!(journal.since(0).events.len(), RETAIN_EVENTS);
        assert!(journal.since(journal.sequence).events.is_empty());
    }

    #[test]
    fn large_event_history_is_paged_without_losing_cursor_positions() {
        let temp = tempfile::tempdir().unwrap();
        let mut journal = EventJournal::new("instance".into(), temp.path().join("events.jsonl"));
        for _ in 0..128 {
            journal.record(None, "x".repeat(5000));
        }
        let first = journal.since(0);
        assert!(first.has_more);
        assert!(serde_json::to_vec(&first).unwrap().len() < 300 * 1024);
        let second = journal.since(first.next_sequence);
        let third = journal.since(second.next_sequence);
        assert!(!third.has_more);
        assert_eq!(
            first.events.len() + second.events.len() + third.events.len(),
            128
        );
    }
}

#[cfg(test)]
#[path = "event_postfix_tests.rs"]
mod postfix_tests;
