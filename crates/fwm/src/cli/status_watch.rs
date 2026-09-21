//! Runtime communication failures are updates in a watch, not its termination.
use super::*;
use crate::client::DaemonPresence;
use fwm_core::model::RuntimeState;

struct View {
    config: Config,
    snapshot: StatusSnapshot,
    state: &'static str,
    warnings: Vec<String>,
}

async fn load(paths: &Paths, selection: Option<fwm_api::protocol::Selection>) -> Result<View> {
    match client::presence(paths).await? {
        DaemonPresence::Running => {
            let view: fwm_api::protocol::StatusView =
                client::decode(client::request(paths, Command::StatusView { selection }).await?)?;
            let config = view.config;
            let snapshot = view.snapshot;
            if config.revision != snapshot.config_revision {
                return Err(client::ClientError {
                    code: "revision_conflict".into(),
                    message: "daemon returned inconsistent configuration and status revisions"
                        .into(),
                }
                .into());
            }
            Ok(View {
                config,
                snapshot,
                state: "running",
                warnings: vec![],
            })
        }
        presence => {
            let loaded = Store::new(paths.clone()).load()?;
            let mut warnings: Vec<_> = loaded.warning.into_iter().collect();
            let state = if matches!(presence, DaemonPresence::Stopped) {
                "stopped"
            } else {
                "unresponsive"
            };
            if state == "unresponsive" {
                warnings.push("daemon exists but is not responding; showing saved configuration, not live connection state".into());
            }
            let mut snapshot = output::offline(loaded.config.clone());
            if state == "unresponsive" {
                mark_unavailable(&mut snapshot, &warnings);
            }
            Ok(View {
                config: loaded.config,
                snapshot,
                state,
                warnings,
            })
        }
    }
}

fn mark_unavailable(snapshot: &mut StatusSnapshot, warnings: &[String]) {
    for rule in &mut snapshot.forwards {
        rule.state = RuntimeState::Unverified;
        rule.last_error = Some(warnings.join("; "));
    }
}

pub async fn status(
    paths: &Paths,
    selection: QueryArgs,
    watch: bool,
    json_output: bool,
) -> Result<()> {
    let server_selection = if watch {
        None
    } else {
        use fwm_api::protocol::Selection;
        Some(if let Some(name) = &selection.name {
            Selection::Forward(name.clone())
        } else if let Some(name) = &selection.server {
            Selection::Server(name.clone())
        } else if let Some(name) = &selection.group {
            Selection::Group(name.clone())
        } else {
            Selection::All
        })
    };
    let interrupted = tokio::signal::ctrl_c();
    tokio::pin!(interrupted);
    let mut watched = None;
    let mut cached = None;
    loop {
        let result = if watch {
            tokio::select! {
                _ = &mut interrupted => return Ok(()),
                result = tokio::time::timeout(Duration::from_secs(3), load(paths, server_selection.clone())) =>
                    result.unwrap_or_else(|_| Err(client::ClientError { code: "ipc_timeout".into(), message: "status refresh timed out; continuing to watch for daemon recovery".into() }.into())),
            }
        } else {
            load(paths, server_selection.clone()).await
        };
        let mut view = match result {
            Ok(view) => {
                cached = Some(view.config.clone());
                view
            }
            Err(error) if watch => {
                let warnings = vec![format!(
                    "status temporarily unavailable: {error:#}; continuing to watch"
                )];
                let config = cached.clone().or_else(|| {
                    Store::new(paths.clone())
                        .load()
                        .ok()
                        .map(|loaded| loaded.config)
                });
                if let Some(config) = config {
                    let mut snapshot = output::offline(config.clone());
                    mark_unavailable(&mut snapshot, &warnings);
                    View {
                        config,
                        snapshot,
                        state: "unavailable",
                        warnings,
                    }
                } else {
                    if json_output {
                        output::json_value(
                            &serde_json::json!({"daemon_running":null,"daemon_state":"unavailable","runtime_available":false,"warnings":warnings}),
                        )?;
                    } else {
                        eprintln!("{}", warnings[0]);
                    }
                    tokio::select! { _=&mut interrupted=>return Ok(()), _=tokio::time::sleep(Duration::from_secs(1))=>{} }
                    continue;
                }
            }
            Err(error) => return Err(error),
        };
        if watch {
            // Invalid selectors are rejected on the first readable config. Once
            // resolved, retain stable identity over outages and renames.
            if watched.is_none() {
                watched = Some(WatchSelection::resolve(&view.config, &selection)?);
            }
            if let Some(watched) = &watched {
                watched.retain(&mut view.snapshot, &view.config);
            } else {
                view.snapshot.forwards.clear();
            }
        } else {
            filter_status(&mut view.snapshot, &view.config, &selection)?;
        }
        output::status_with_state(&view.snapshot, view.state, json_output, &view.warnings)?;
        if !watch {
            return Ok(());
        }
        tokio::select! { _=&mut interrupted=>return Ok(()), _=tokio::time::sleep(Duration::from_secs(1))=>{} }
    }
}
