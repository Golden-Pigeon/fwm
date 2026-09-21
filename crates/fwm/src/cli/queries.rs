use super::{args::QueryArgs, output};
use crate::client;
use anyhow::Result;
use fwm_api::protocol::{Command, StatusSnapshot};
use fwm_core::{
    history::{self, HistoryCursor, HistoryEntry, HistoryFilter, HistoryRead},
    model::Config,
    paths::Paths,
    store::Store,
};
use std::{collections::HashSet, time::Duration};

#[path = "query_selection.rs"]
mod streaming_selection;
use streaming_selection::{FollowFilter, WatchSelection};

#[path = "status_watch.rs"]
mod status_watch;
pub use status_watch::status;

fn filter_status(
    snapshot: &mut StatusSnapshot,
    config: &Config,
    selection: &QueryArgs,
) -> Result<()> {
    let ids = if let Some(name) = &selection.name {
        config.select_forwards(name)
    } else if let Some(server) = &selection.server {
        config.select_server_forwards(server)
    } else if let Some(group) = &selection.group {
        config.select_group_forwards(group)
    } else {
        return Ok(());
    }
    .map_err(|message| client::ClientError {
        code: "not_found".into(),
        message,
    })?;
    let selected: HashSet<_> = ids.into_iter().collect();
    snapshot
        .forwards
        .retain(|forward| selected.contains(&forward.id));
    Ok(())
}

/// Read persisted events directly. Log queries never need an IPC connection and
/// never start the daemon, including queries for deleted rules or old names.
pub async fn logs(
    paths: &Paths,
    selection: QueryArgs,
    follow: bool,
    tail: usize,
    json_output: bool,
) -> Result<()> {
    let mut filter = FollowFilter::new(HistoryFilter {
        name: selection.name,
        server: selection.server,
        group: selection.group,
    });
    let (initial, config) = load_history(paths)?;
    let selected = history::tail(filter.select(&initial.entries, config.as_ref()), tail);
    if !follow {
        if json_output {
            output::json_value(
                &serde_json::json!({"events":selected,"warnings":initial.warnings}),
            )?;
        } else {
            for warning in &initial.warnings {
                eprintln!("warning: {warning}");
            }
            print_entries(&selected, false)?;
            if selected.is_empty() {
                println!(
                    "No matching retained events in {}.",
                    paths.state_dir.join("events.jsonl").display()
                );
            }
        }
        return Ok(());
    }
    let mut warnings_seen = HashSet::new();
    report_new_warnings(&initial.warnings, &mut warnings_seen);
    print_entries(&selected, json_output)?;
    let mut cursor = HistoryCursor::from_snapshot(&initial.entries);
    loop {
        tokio::select! { _ = tokio::signal::ctrl_c() => return Ok(()), _ = tokio::time::sleep(Duration::from_millis(500)) => {} }
        let (history, config) = load_history(paths)?;
        report_new_warnings(&history.warnings, &mut warnings_seen);
        let (fresh, gap) = cursor.take_new(&history.entries);
        if gap {
            eprintln!(
                "warning: some events expired during log rotation; continuing from the retained history"
            );
        }
        let keys: HashSet<_> = fresh
            .iter()
            .map(|entry| (entry.daemon_instance_id.as_str(), entry.event.sequence))
            .collect();
        // Resolve historical aliases against all retained entries, then select
        // only unseen events. Resolving against fresh alone loses old names
        // immediately after a rename or daemon restart.
        let selected = filter
            .select(&history.entries, config.as_ref())
            .into_iter()
            .filter(|entry| {
                keys.contains(&(entry.daemon_instance_id.as_str(), entry.event.sequence))
            })
            .collect::<Vec<_>>();
        print_entries(&selected, json_output)?;
    }
}

fn load_history(paths: &Paths) -> Result<(HistoryRead, Option<Config>)> {
    let mut history = history::read_history(&paths.state_dir.join("events.jsonl"))?;
    let config = match Store::new(paths.clone()).load() {
        Ok(loaded) => {
            if let Some(warning) = loaded.warning {
                history.warnings.push(warning);
            }
            Some(loaded.config)
        }
        Err(error) => {
            history.warnings.push(format!("saved configuration could not be read ({error:#}); using labels recorded in event history"));
            None
        }
    };
    history::enrich_legacy(&mut history.entries, config.as_ref());
    let unlabeled = history
        .entries
        .iter()
        .filter(|entry| entry.event.forward_id.is_some() && entry.forward_name.is_none())
        .count();
    if unlabeled != 0 {
        history.warnings.push(format!("{unlabeled} legacy event(s) have no retained rule names; query them by rule ID or without a name filter"));
    }
    Ok((history, config))
}

fn report_new_warnings(warnings: &[String], seen: &mut HashSet<String>) {
    // Bound retained warning text for a long-running watcher.
    if seen.len() > 128 {
        seen.clear();
    }
    for warning in warnings {
        if seen.insert(warning.clone()) {
            eprintln!("warning: {warning}");
        }
    }
}

fn print_entries(entries: &[HistoryEntry], json_output: bool) -> Result<()> {
    for entry in entries {
        if json_output {
            output::json_value(&serde_json::to_value(entry)?)?;
        } else {
            let scope = match (&entry.server_name, &entry.group) {
                (Some(server), Some(group)) => format!(" [{server} / {group}]"),
                (Some(server), None) => format!(" [{server}]"),
                (None, Some(group)) => format!(" [group: {group}]"),
                (None, None) => String::new(),
            };
            println!(
                "{}  {}{}  {}",
                history::format_timestamp(entry.event.timestamp_ms),
                entry.label(),
                scope,
                entry.event.message
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fwm_core::model::{
        ConnectionMode, DesiredState, EngineEvent, ForwardSpec, RemoteCleanup, ServerProfile,
        Tunnel,
    };

    fn config() -> Config {
        let mut server = ServerProfile::new("dev");
        server.host = Some("127.0.0.1".into());
        Config {
            forwards: (3000..3003)
                .map(|port| ForwardSpec {
                    id: format!("id-{port}"),
                    name: format!("web-{port}"),
                    server_id: server.id.clone(),
                    group: Some("web".into()),
                    tunnel: Tunnel::Local {
                        listen: format!("127.0.0.1:{port}").parse().unwrap(),
                        target: "localhost:80".parse().unwrap(),
                    },
                    desired_state: DesiredState::Stopped,
                    connection_mode: ConnectionMode::Shared,
                    remote_cleanup: RemoteCleanup::Off,
                })
                .collect(),
            servers: vec![server],
            ..Default::default()
        }
    }

    #[test]
    fn status_selectors_match_exact_rules_groups_and_servers() {
        let config = config();
        for (query, expected) in [
            (
                QueryArgs {
                    name: Some("web-3001".into()),
                    ..Default::default()
                },
                1,
            ),
            (
                QueryArgs {
                    name: Some("web".into()),
                    ..Default::default()
                },
                3,
            ),
            (
                QueryArgs {
                    group: Some("web".into()),
                    ..Default::default()
                },
                3,
            ),
            (
                QueryArgs {
                    server: Some("dev".into()),
                    ..Default::default()
                },
                3,
            ),
        ] {
            let mut snapshot = output::offline(config.clone());
            filter_status(&mut snapshot, &config, &query).unwrap();
            assert_eq!(snapshot.forwards.len(), expected);
        }
        assert!(
            filter_status(
                &mut output::offline(config.clone()),
                &config,
                &QueryArgs {
                    name: Some("missing".into()),
                    ..Default::default()
                }
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn logs_reads_deleted_names_offline_without_starting_or_contacting_a_daemon() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(directory.path().to_path_buf())).unwrap();
        paths.ensure_dirs().unwrap();
        let config = config();
        let entry = HistoryEntry::for_rule(
            "previous-daemon".into(),
            EngineEvent {
                context: None,
                server_id: None,
                sequence: 1,
                timestamp_ms: 0,
                forward_id: Some(config.forwards[0].id.clone()),
                message: "old failure".into(),
            },
            &config.forwards[0],
            &config.servers[0],
        );
        history::append_history(&paths.state_dir.join("events.jsonl"), &entry).unwrap();
        let (loaded, current) = load_history(&paths).unwrap();
        assert!(current.unwrap().forwards.is_empty());
        assert_eq!(loaded.entries[0].forward_name.as_deref(), Some("web-3000"));
        logs(
            &paths,
            QueryArgs {
                name: Some("web-3000".into()),
                ..Default::default()
            },
            false,
            100,
            true,
        )
        .await
        .unwrap();
        assert!(
            !paths.lock_file.exists(),
            "log reading must not launch a daemon"
        );
        assert!(
            !paths.config_file.exists(),
            "log reading must not initialize config"
        );
    }
}
