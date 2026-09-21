use super::HistoryEntry;
use std::collections::{HashMap, HashSet};

/// Deduplicates replayed retained files across rotations and daemon restarts.
/// Sequence numbers are only meaningful within their daemon instance.
#[derive(Default)]
pub struct HistoryCursor {
    instances: HashMap<String, u64>,
}

impl HistoryCursor {
    pub fn from_snapshot(entries: &[HistoryEntry]) -> Self {
        let mut cursor = Self::default();
        for entry in entries {
            cursor
                .instances
                .entry(entry.daemon_instance_id.clone())
                .and_modify(|sequence| *sequence = (*sequence).max(entry.event.sequence))
                .or_insert(entry.event.sequence);
        }
        cursor
    }

    pub fn take_new(&mut self, entries: &[HistoryEntry]) -> (Vec<HistoryEntry>, bool) {
        let mut fresh = Vec::new();
        let mut gap = false;
        let mut retained = HashSet::new();
        for entry in entries {
            retained.insert(entry.daemon_instance_id.clone());
            let previous = self
                .instances
                .entry(entry.daemon_instance_id.clone())
                .or_insert(0);
            if entry.event.sequence > *previous {
                gap |= entry.event.sequence > previous.saturating_add(1);
                *previous = entry.event.sequence;
                fresh.push(entry.clone());
            }
        }
        if !retained.is_empty() {
            self.instances
                .retain(|instance, _| retained.contains(instance));
        }
        (fresh, gap)
    }
}
