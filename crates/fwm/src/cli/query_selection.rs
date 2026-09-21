//! Keep streaming selections meaningful while names, membership and retained
//! history change. One-shot queries still validate the current selector.
use super::*;

pub(super) enum WatchSelection {
    All,
    Forward(String),
    Server(String),
    Group(String),
}

impl WatchSelection {
    pub fn resolve(config: &Config, selection: &QueryArgs) -> Result<Self> {
        let missing = |message: String| client::ClientError {
            code: "not_found".into(),
            message,
        };
        if let Some(name) = &selection.name {
            if let Some(forward) = config.forward(name) {
                return Ok(Self::Forward(forward.id.clone()));
            }
            config.select_group_forwards(name).map_err(missing)?;
            return Ok(Self::Group(name.clone()));
        }
        if let Some(name) = &selection.server {
            let server = config
                .server(name)
                .ok_or_else(|| missing(format!("unknown server {name:?}")))?;
            return Ok(Self::Server(server.id.clone()));
        }
        if let Some(group) = &selection.group {
            config.select_group_forwards(group).map_err(missing)?;
            return Ok(Self::Group(group.clone()));
        }
        Ok(Self::All)
    }

    pub fn retain(&self, snapshot: &mut StatusSnapshot, config: &Config) {
        snapshot.forwards.retain(|status| match self {
            Self::All => true,
            Self::Forward(id) => &status.id == id,
            Self::Server(id) => config
                .forward(&status.id)
                .is_some_and(|rule| &rule.server_id == id),
            Self::Group(group) => config
                .forward(&status.id)
                .is_some_and(|rule| rule.group.as_ref() == Some(group)),
        });
    }
}

pub(super) struct FollowFilter {
    filter: HistoryFilter,
    known_ids: HashSet<String>,
    server_id: Option<String>,
    resolved_name: bool,
}

impl FollowFilter {
    pub fn new(filter: HistoryFilter) -> Self {
        Self {
            filter,
            known_ids: HashSet::new(),
            server_id: None,
            resolved_name: false,
        }
    }

    pub fn select(
        &mut self,
        entries: &[HistoryEntry],
        config: Option<&Config>,
    ) -> Vec<HistoryEntry> {
        if self.server_id.is_none()
            && let Some(name) = &self.filter.server
        {
            self.server_id = config
                .and_then(|config| config.server(name))
                .map(|server| server.id.clone())
                .or_else(|| {
                    entries
                        .iter()
                        .any(|entry| entry.server_id.as_ref() == Some(name))
                        .then(|| name.clone())
                })
                .or_else(|| {
                    let ids: HashSet<_> = entries
                        .iter()
                        .filter(|entry| {
                            entry.server_name.as_ref() == Some(name)
                                || entry.server_id.as_ref() == Some(name)
                        })
                        .filter_map(|entry| entry.server_id.clone())
                        .collect();
                    (ids.len() == 1).then(|| ids.into_iter().next().unwrap())
                });
        }
        if !self.resolved_name
            && let Some(name) = &self.filter.name
        {
            let historical_ids: HashSet<_> = entries
                .iter()
                .filter(|entry| {
                    entry.forward_name.as_ref() == Some(name)
                        || entry.event.forward_id.as_ref() == Some(name)
                })
                .filter_map(|entry| entry.event.forward_id.clone())
                .collect();
            if let Some(rule) = config.and_then(|config| config.forward(name)) {
                self.known_ids.insert(rule.id.clone());
                self.resolved_name = true;
            } else if entries
                .iter()
                .any(|entry| entry.event.forward_id.as_ref() == Some(name))
            {
                self.known_ids.insert(name.clone());
                self.resolved_name = true;
            } else if config.is_some_and(|config| {
                config
                    .forwards
                    .iter()
                    .any(|rule| rule.group.as_ref() == Some(name))
            }) || (historical_ids.is_empty()
                && entries
                    .iter()
                    .any(|entry| entry.group.as_ref() == Some(name)))
            {
                self.filter.group = Some(name.clone());
                self.filter.name = None;
                self.resolved_name = true;
            } else if !historical_ids.is_empty() {
                self.known_ids = historical_ids;
                self.resolved_name = true;
            }
        }
        let mut base = self.filter.clone();
        if self.server_id.is_some() {
            base.server = None;
        }
        if self.filter.name.is_none() {
            return self.retain_server(base.select(entries, config));
        }
        // Retain IDs once learned: the original name may no longer occur in
        // either bounded history file after a rename, deletion and rotation.
        let remaining = HistoryFilter {
            name: None,
            server: base.server,
            group: self.filter.group.clone(),
        };
        let selected = remaining
            .select(entries, config)
            .into_iter()
            .filter(|entry| {
                entry
                    .event
                    .forward_id
                    .as_ref()
                    .is_some_and(|id| self.known_ids.contains(id))
            })
            .collect();
        self.retain_server(selected)
    }

    fn retain_server(&self, mut entries: Vec<HistoryEntry>) -> Vec<HistoryEntry> {
        if let Some(id) = &self.server_id {
            entries.retain(|entry| entry.server_id.as_ref() == Some(id));
        }
        entries
    }
}

#[cfg(test)]
#[path = "query_selection_tests.rs"]
mod tests;
