//! Submit configuration changes without waking a stopped daemon.
mod history;
mod recovery;
pub(crate) use recovery::recover;
#[cfg(test)]
mod tests;
use crate::{client, configuration};
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use fwm_api::protocol::{Command, MutationReply, Response};
use fwm_core::{paths::Paths, store::Store};
use std::{fs::OpenOptions, time::Duration};

pub async fn control(paths: &Paths, command: Command) -> Result<Response> {
    mutate(paths, command, None).await
}

pub async fn query(paths: &Paths, command: Command) -> Result<Response> {
    match client::presence(paths).await? {
        client::DaemonPresence::Running => {
            let mut response = client::request(paths, command).await?;
            crate::ssh_actions::diagnostic_recovery(paths, &mut response.data);
            return Ok(response);
        },
        client::DaemonPresence::Unresponsive => return Err(client::ClientError { code: "daemon_unresponsive".into(), message: "daemon exists but is not responding; inspect `fwm daemon status` and the daemon log before retrying diagnostics".into() }.into()),
        client::DaemonPresence::Stopped => {},
    }
    let mut data = crate::ssh_actions::offline(paths, command)
        .await
        .map_err(api_error)?;
    crate::ssh_actions::diagnostic_recovery(paths, &mut data);
    Ok(Response::success(uuid::Uuid::new_v4().to_string(), data))
}

pub fn api_error(error: fwm_api::protocol::ApiError) -> anyhow::Error {
    client::ClientError {
        code: error.code,
        message: error.message,
    }
    .into()
}

pub async fn mutate(paths: &Paths, command: Command, revision: Option<u64>) -> Result<Response> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    paths.ensure_dirs()?;
    loop {
        if client::running(paths).await {
            return client::send(paths, command, revision).await;
        }
        if std::fs::symlink_metadata(&paths.lock_file).is_ok_and(|m| m.file_type().is_symlink()) {
            bail!("refusing symlink daemon lock");
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(&paths.lock_file)?;
        match lock.try_lock_exclusive() {
            Ok(()) => return apply_locked(paths, command, revision),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(client::ClientError { code: "daemon_unresponsive".into(), message:
                "daemon owns this configuration but is not responding; no offline change was made".into() }.into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn apply_locked(paths: &Paths, command: Command, revision: Option<u64>) -> Result<Response> {
    let store = Store::new(paths.clone());
    let before = store.load()?.config;
    if revision.is_some_and(|revision| revision != before.revision) {
        return Err(api_error(fwm_api::protocol::ApiError::new(
            "revision_conflict",
            format!(
                "current revision is {}; retry your command",
                before.revision
            ),
        )));
    }
    let reload = matches!(command, Command::Reload);
    let change = if reload {
        configuration::Change {
            config: store.read_candidate()?,
            selected: None,
            message: "configuration reloaded".into(),
        }
    } else {
        configuration::prepare(&before, &command).map_err(api_error)?
    };
    let mut config = change.config;
    let mut message = change.message;
    if !reload && change.selected.is_none() && config == before {
        message = "No changes; daemon remains stopped.".into();
    } else {
        if !reload && change.selected.is_none() && store.has_pending_edits()? {
            return Err(api_error(fwm_api::protocol::ApiError::new(
                "config_pending_edits",
                "config.toml has unapplied edits; validate and reload it before changing rules",
            )));
        }
        config.revision = before
            .revision
            .checked_add(1)
            .context("configuration revision exhausted")?;
        store.initialize(&before)?;
        let warning = if reload {
            store.commit_reload(&config)?
        } else if let Some(ids) = &change.selected {
            store.commit_control_for(&config, ids)?
        } else {
            store.commit(&config)?
        };
        message.push_str("; daemon remains stopped.");
        if let Some(warning) = warning {
            message.push_str(&format!(" {warning}"));
        }
        history::record(
            paths,
            &before,
            &config,
            change.selected.as_deref(),
            &mut message,
        );
    }
    let reply = MutationReply {
        revision: config.revision,
        message,
        config,
        operation: None,
    };
    Ok(Response::success(
        uuid::Uuid::new_v4().to_string(),
        serde_json::to_value(reply)?,
    ))
}
