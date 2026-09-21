//! Transport glue. The versioned request/response types live in fwm-api.
use crate::platform::{background, ipc, service};
use anyhow::{Context, Result, anyhow};
use fwm_api::client::Client;
use fwm_api::protocol::{Command, Request, Response};
use fwm_core::{model::RemoteCleanup, paths::Paths};
use serde::de::DeserializeOwned;
use std::time::Duration;

#[derive(Debug)]
pub struct ClientError {
    pub code: String,
    pub message: String,
}
impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for ClientError {}

pub async fn request(paths: &Paths, command: Command) -> Result<Response> {
    send(paths, command, None).await
}

pub async fn send(paths: &Paths, command: Command, revision: Option<u64>) -> Result<Response> {
    let mut request = Request::new(uuid::Uuid::new_v4().to_string(), command);
    request.expected_revision = revision;
    let ux_command = matches!(
        &request.command,
        Command::PutServer { .. }
            | Command::RemoveServer { .. }
            | Command::PutForward { .. }
            | Command::CreateForwards { .. }
            | Command::PutForwardWithServer { .. }
            | Command::PutForwardsWithServer { .. }
            | Command::RemoveForwards { .. }
            | Command::Restart { .. }
            | Command::Retry { .. }
            | Command::SetDesired { .. }
            | Command::Reload
            | Command::InspectHostProfile { .. }
            | Command::TrustHostProfile { .. }
            | Command::InspectHopProfile { .. }
            | Command::TrustHopProfile { .. }
            | Command::DoctorProfile { .. }
            | Command::Doctor { .. }
    );
    let grouped = matches!(&request.command, Command::CreateForwards { forwards, .. }
        if forwards.iter().any(|forward| forward.group.is_some()));
    if ux_command || grouped {
        let ping = Request::new(uuid::Uuid::new_v4().to_string(), Command::Ping);
        require_capability(&exchange(paths, &ping).await?, paths, "cli_ux_v5")?;
    }
    if matches!(&request.command, Command::StatusView { .. }) {
        let ping = Request::new(uuid::Uuid::new_v4().to_string(), Command::Ping);
        require_capability(&exchange(paths, &ping).await?, paths, "atomic_status_view")?;
    }
    // Older daemons accepted CreateForwards before it supported an embedded
    // server. Serde would ignore the extra field and report an unrelated
    // missing-server error, so detect this before submitting any mutation.
    if matches!(
        request.command,
        Command::CreateForwards {
            server: Some(_),
            ..
        }
    ) {
        let ping = Request::new(uuid::Uuid::new_v4().to_string(), Command::Ping);
        let capabilities = exchange(paths, &ping).await?;
        require_capability(&capabilities, paths, "atomic_alias_add")?;
    }
    let verified = match &request.command {
        Command::CreateForwards { forwards, .. }
        | Command::PutForwardsWithServer { forwards, .. } => forwards
            .iter()
            .any(|rule| rule.remote_cleanup == RemoteCleanup::Verified),
        Command::PutForward { forward } | Command::PutForwardWithServer { forward, .. } => {
            forward.remote_cleanup == RemoteCleanup::Verified
        }
        _ => false,
    };
    if verified {
        let ping = Request::new(uuid::Uuid::new_v4().to_string(), Command::Ping);
        require_capability(
            &exchange(paths, &ping).await?,
            paths,
            "verified_remote_cleanup",
        )?;
    }
    exchange(paths, &request).await
}

fn require_capability(response: &Response, paths: &Paths, capability: &str) -> Result<()> {
    if response
        .data
        .get("capabilities")
        .and_then(|value| value.as_array())
        .is_some_and(|values| {
            values
                .iter()
                .any(|value| value.as_str() == Some(capability))
        })
    {
        return Ok(());
    }
    Err(ClientError {
        code: "daemon_upgrade_required".into(),
        message: format!(
            "the running daemon is from an older build and lacks {capability}. Run {}, then repeat your command; saved rules and running intent are retained",
            crate::ssh_actions::command_line(paths, &["daemon", "restart"])
        ),
    }.into())
}

async fn exchange(paths: &Paths, request: &Request) -> Result<Response> {
    let response = tokio::time::timeout(Duration::from_secs(45), async {
        let stream = ipc::connect(paths).await.context(
            "daemon is not running or its private socket is unavailable; run `fwm daemon start`",
        )?;
        let response = Client::new(stream)
            .call(request)
            .await
            .context("exchanging daemon request")?;
        Ok::<_, anyhow::Error>(response)
    })
    .await
    .map_err(|_| ClientError {
        code: "ipc_timeout".into(),
        message: "daemon did not respond within 45s; the submitted operation may still complete"
            .into(),
    })??;
    if !response.ok {
        let error = response
            .error
            .ok_or_else(|| anyhow!("daemon returned an error without details"))?;
        return Err(ClientError {
            code: error.code,
            message: error.message,
        }
        .into());
    }
    Ok(response)
}

pub fn decode<T: DeserializeOwned>(response: Response) -> Result<T> {
    serde_json::from_value(response.data).context("invalid daemon response payload")
}

pub async fn running(paths: &Paths) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(1), request(paths, Command::Ping)).await,
        Ok(Ok(_))
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonPresence {
    Running,
    Stopped,
    Unresponsive,
}

pub async fn presence(paths: &Paths) -> Result<DaemonPresence> {
    if running(paths).await {
        return Ok(DaemonPresence::Running);
    }
    if instance_lock_held(paths)? {
        return Ok(DaemonPresence::Unresponsive);
    }
    // A listening peer with an incompatible or broken protocol is still an
    // owner, not permission to launch another instance over its socket.
    if matches!(
        tokio::time::timeout(Duration::from_millis(100), ipc::connect(paths)).await,
        Ok(Ok(_))
    ) {
        return Ok(DaemonPresence::Unresponsive);
    }
    Ok(DaemonPresence::Stopped)
}

pub fn instance_lock_held(paths: &Paths) -> Result<bool> {
    if std::fs::symlink_metadata(&paths.lock_file)
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(anyhow!("refusing symlink daemon lock"));
    }
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&paths.lock_file)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            fs2::FileExt::unlock(&file)?;
            Ok(false)
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn unresponsive_message(message: String) -> String {
    #[cfg(windows)]
    let message = format!(
        "{message}. If this began after an upgrade, repeat the original --config-dir spelling so the legacy daemon pipe can be found; unrelated profile pipes are never searched"
    );
    message
}

fn unresponsive_start_error() -> anyhow::Error {
    ClientError {
        code: "daemon_unresponsive".into(),
        message: unresponsive_message("a daemon owns this profile but is not responding; no second daemon was started. Try daemon stop, then daemon start, and inspect the daemon log if the owner cannot stop".into()),
    }.into()
}

pub async fn ensure_running(paths: &Paths) -> Result<()> {
    match presence(paths).await? {
        DaemonPresence::Running => return Ok(()),
        DaemonPresence::Unresponsive => return Err(unresponsive_start_error()),
        DaemonPresence::Stopped => {}
    }
    paths.ensure_dirs()?;
    let mut operation = service::Operation::acquire(paths)?;
    // Serialize the check and launch with install/stop/restart of this profile.
    match presence(paths).await? {
        DaemonPresence::Running => return Ok(()),
        DaemonPresence::Unresponsive => return Err(unresponsive_start_error()),
        DaemonPresence::Stopped => {}
    }
    if operation.installed()? {
        operation.start()?;
    } else {
        background::spawn(paths)?;
    }
    wait_running(paths, Duration::from_secs(5)).await
}

pub(crate) async fn wait_running(paths: &Paths, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if running(paths).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(ClientError {
        code: "daemon_unavailable".into(),
        message: format!(
            "daemon did not become ready; inspect {}",
            paths.log_file.display()
        ),
    }
    .into())
}

#[cfg(test)]
#[path = "client_failure_tests.rs"]
mod failure_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn old_daemon_gets_actionable_upgrade_error_before_alias_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(directory.path().to_owned())).unwrap();
        let old = Response::success("ping".into(), json!({"version":"0.1.0"}));
        let error = require_capability(&old, &paths, "atomic_alias_add").unwrap_err();
        assert_eq!(
            error.downcast_ref::<ClientError>().unwrap().code,
            "daemon_upgrade_required"
        );
        assert!(
            error
                .to_string()
                .contains(&crate::ssh_actions::command_line(
                    &paths,
                    &["daemon", "restart"]
                ))
        );
        let current =
            Response::success("ping".into(), json!({"capabilities":["atomic_alias_add"]}));
        require_capability(&current, &paths, "atomic_alias_add").unwrap();
    }
}
