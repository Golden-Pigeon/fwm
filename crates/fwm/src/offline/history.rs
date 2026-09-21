//! Label offline changes with the same identities used by online event history.
use fwm_core::{
    history::{HistoryEntry, append_history},
    model::{Config, EngineEvent, unix_ms},
    paths::Paths,
};

pub(super) fn record(
    paths: &Paths,
    before: &Config,
    after: &Config,
    selected: Option<&[String]>,
    message: &mut String,
) {
    let ids = selected.map(<[String]>::to_vec).unwrap_or_else(|| {
        before
            .forwards
            .iter()
            .chain(after.forwards.iter())
            .filter(|f| before.forward(&f.id) != after.forward(&f.id))
            .map(|f| f.id.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    });
    let instance = format!("offline-{}", uuid::Uuid::new_v4());
    let mut entries = Vec::new();
    for id in &ids {
        if let Some(rule) = after.forward(id).or_else(|| before.forward(id))
            && let Some(server) = after
                .server(&rule.server_id)
                .or_else(|| before.server(&rule.server_id))
        {
            entries.push(HistoryEntry::for_rule(
                instance.clone(),
                EngineEvent {
                    context: None,
                    sequence: entries.len() as u64 + 1,
                    timestamp_ms: unix_ms(),
                    forward_id: Some(id.clone()),
                    server_id: Some(server.id.clone()),
                    message: message.clone(),
                },
                rule,
                server,
            ));
        }
    }
    let changed_servers: std::collections::BTreeSet<_> = before
        .servers
        .iter()
        .chain(after.servers.iter())
        .filter(|server| before.server(&server.id) != after.server(&server.id))
        .map(|server| server.id.clone())
        .collect();
    for id in changed_servers {
        if let Some(server) = after.server(&id).or_else(|| before.server(&id)) {
            entries.push(HistoryEntry::for_server(
                instance.clone(),
                EngineEvent {
                    context: None,
                    sequence: entries.len() as u64 + 1,
                    timestamp_ms: unix_ms(),
                    forward_id: None,
                    server_id: Some(id),
                    message: message.clone(),
                },
                server,
            ));
        }
    }
    if entries.is_empty() {
        entries.push(HistoryEntry::new(
            instance,
            EngineEvent {
                context: None,
                sequence: 1,
                timestamp_ms: unix_ms(),
                forward_id: None,
                server_id: None,
                message: message.clone(),
            },
        ));
    }
    for entry in entries {
        if let Err(error) = append_history(&paths.state_dir.join("events.jsonl"), &entry) {
            message.push_str(&format!(" Event log write failed: {error}."));
            break;
        }
    }
}
