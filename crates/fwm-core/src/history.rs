//! Bounded, durable event history shared by the daemon and offline clients.
//!
//! Event labels are stored at observation time so removing or renaming a rule
//! cannot erase the identity needed to query its earlier diagnostics.

mod cursor;
mod storage;

pub use cursor::HistoryCursor;
pub use storage::{append_history, read_history, rotated_path};

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::model::{Config, EngineEvent, EventContext, ForwardSpec, ServerProfile};

pub const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_RECORD_BYTES: usize = 8 * 1024;
pub const MAX_MESSAGE_BYTES: usize = 4096;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    #[serde(default)]
    pub daemon_instance_id: String,
    pub event: EngineEvent,
    #[serde(default)]
    pub forward_name: Option<String>,
    #[serde(default)]
    pub server_id: Option<String>,
    #[serde(default)]
    pub server_name: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct HistoryLabels {
    pub forward_name: Option<String>,
    pub server_id: Option<String>,
    pub server_name: Option<String>,
    pub group: Option<String>,
}

impl HistoryLabels {
    pub fn from_entry(entry: &HistoryEntry) -> Self {
        Self {
            forward_name: entry.forward_name.clone(),
            server_id: entry.server_id.clone(),
            server_name: entry.server_name.clone(),
            group: entry.group.clone(),
        }
    }

    pub fn apply(&self, entry: &mut HistoryEntry) {
        entry.forward_name = self.forward_name.clone();
        entry.server_id = self.server_id.clone();
        entry.server_name = self.server_name.clone();
        entry.group = self.group.clone();
    }
}

pub fn labels_from_config(config: &Config) -> HashMap<String, HistoryLabels> {
    config
        .forwards
        .iter()
        .map(|forward| {
            let server = config.server(&forward.server_id);
            (
                forward.id.clone(),
                HistoryLabels {
                    forward_name: Some(forward.name.clone()),
                    server_id: Some(forward.server_id.clone()),
                    server_name: server.map(|server| server.name.clone()),
                    group: forward.group.clone(),
                },
            )
        })
        .collect()
}

impl HistoryEntry {
    pub fn new(instance: String, mut event: EngineEvent) -> Self {
        let context = event.context.clone().unwrap_or_default();
        event.server_id = context.server_id.clone().or(event.server_id);
        Self {
            daemon_instance_id: instance,
            server_id: event.server_id.clone(),
            event,
            forward_name: context.forward_name,
            server_name: context.server_name,
            group: context.group,
        }
    }

    pub fn label(&self) -> &str {
        self.forward_name
            .as_deref()
            .or(self.event.forward_id.as_deref())
            .or(self.server_name.as_deref())
            .or(self.server_id.as_deref())
            .unwrap_or("daemon")
    }

    pub fn for_rule(
        instance: String,
        mut event: EngineEvent,
        forward: &ForwardSpec,
        server: &ServerProfile,
    ) -> Self {
        if event.context.is_none() {
            event.context = Some(EventContext {
                forward_name: Some(forward.name.clone()),
                server_id: Some(server.id.clone()),
                server_name: Some(server.name.clone()),
                group: forward.group.clone(),
            });
        }
        Self::new(instance, event)
    }

    pub fn for_server(instance: String, mut event: EngineEvent, server: &ServerProfile) -> Self {
        if event.context.is_none() {
            event.context = Some(EventContext {
                forward_name: None,
                server_id: Some(server.id.clone()),
                server_name: Some(server.name.clone()),
                group: None,
            });
        }
        Self::new(instance, event)
    }
}

#[derive(Debug, Default)]
pub struct HistoryRead {
    pub entries: Vec<HistoryEntry>,
    pub warnings: Vec<String>,
}

/// Populate envelope fields only from context captured on the same event.
/// Legacy records without context keep their recorded labels and stable IDs:
/// a current configuration or a later event cannot establish their old scope.
/// Keep the config parameter for callers of the existing public interface.
pub fn enrich_legacy(entries: &mut [HistoryEntry], _config: Option<&Config>) {
    for entry in entries {
        if let Some(context) = &entry.event.context {
            entry.forward_name = context.forward_name.clone();
            entry.server_id = context.server_id.clone().or(entry.event.server_id.clone());
            entry.event.server_id = entry.server_id.clone();
            entry.server_name = context.server_name.clone();
            entry.group = context.group.clone();
        } else if entry.server_id.is_none() {
            entry.server_id = entry.event.server_id.clone();
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct HistoryFilter {
    pub name: Option<String>,
    pub server: Option<String>,
    pub group: Option<String>,
}

impl HistoryFilter {
    /// A name follows the rule's stable ID across renames. Explicit server/group
    /// filters use labels from the time of the event, including deleted scopes.
    pub fn select(&self, entries: &[HistoryEntry], config: Option<&Config>) -> Vec<HistoryEntry> {
        // Current identities have priority over historical aliases. Group
        // shorthand always uses event-time group labels, exactly like --group.
        let mut selected_ids = HashSet::new();
        let mut implicit_group = None;
        if let Some(name) = &self.name {
            if let Some(rule) = config.and_then(|config| config.forward(name)) {
                selected_ids.insert(rule.id.clone());
            } else if entries
                .iter()
                .any(|entry| entry.event.forward_id.as_ref() == Some(name))
            {
                selected_ids.insert(name.clone());
            } else if config.is_some_and(|config| {
                config
                    .forwards
                    .iter()
                    .any(|rule| rule.group.as_ref() == Some(name))
            }) {
                implicit_group = Some(name.as_str());
            } else {
                selected_ids.extend(
                    entries
                        .iter()
                        .filter(|entry| {
                            entry.forward_name.as_ref() == Some(name)
                                || entry.event.forward_id.as_ref() == Some(name)
                        })
                        .filter_map(|entry| entry.event.forward_id.clone()),
                );
                if selected_ids.is_empty() {
                    implicit_group = Some(name.as_str());
                }
            }
        }
        let selected_server_id = self
            .server
            .as_deref()
            .and_then(|name| config.and_then(|config| config.server(name)))
            .map(|server| server.id.as_str())
            .or_else(|| {
                self.server.as_deref().filter(|id| {
                    entries
                        .iter()
                        .any(|entry| entry.server_id.as_deref() == Some(*id))
                })
            });
        entries
            .iter()
            .filter(|entry| {
                self.name.is_none()
                    || implicit_group.is_some_and(|group| entry.group.as_deref() == Some(group))
                    || entry
                        .event
                        .forward_id
                        .as_ref()
                        .is_some_and(|id| selected_ids.contains(id))
            })
            .filter(|entry| {
                self.server.as_ref().is_none_or(|name| {
                    if let Some(id) = selected_server_id {
                        entry.server_id.as_deref() == Some(id)
                    } else {
                        entry.server_name.as_ref() == Some(name)
                    }
                })
            })
            .filter(|entry| {
                self.group
                    .as_ref()
                    .is_none_or(|group| entry.group.as_ref() == Some(group))
            })
            .cloned()
            .collect()
    }
}

pub fn tail(entries: Vec<HistoryEntry>, count: usize) -> Vec<HistoryEntry> {
    let skip = entries.len().saturating_sub(count);
    entries.into_iter().skip(skip).collect()
}

pub fn format_timestamp(timestamp_ms: u64) -> String {
    i64::try_from(timestamp_ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|timestamp| timestamp.format("%Y-%m-%d %H:%M:%S%.3f UTC").to_string())
        .unwrap_or_else(|| "invalid timestamp".into())
}

#[cfg(test)]
mod selection_audit_tests;
#[cfg(test)]
mod tests;
