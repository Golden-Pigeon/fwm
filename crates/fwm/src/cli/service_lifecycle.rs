//! One locked transaction spans manager handoff, definition changes and recovery.
use crate::{
    client::{self, DaemonPresence},
    platform::{background, service},
};
use anyhow::{Context, Result};
use fwm_api::protocol::Command;
use fwm_core::paths::Paths;
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
struct Before {
    running: bool,
    managed: bool,
    installed: bool,
}
trait Control {
    async fn prepare(&mut self, installation: bool) -> Result<Before>;
    fn installed(&mut self) -> Result<bool>;
    fn stop_service(&mut self) -> Result<()>;
    async fn presence(&mut self) -> Result<DaemonPresence>;
    async fn shutdown(&mut self) -> Result<()>;
    async fn wait_stopped(&mut self) -> Result<()>;
    async fn wait_running(&mut self) -> Result<()>;
    fn install(&mut self) -> Result<()>;
    fn refresh(&mut self) -> Result<()>;
    fn spawn(&mut self) -> Result<()>;
    async fn restore(&mut self, before: Before) -> Result<()>;
    fn uninstall(&mut self) -> Result<()>;
}

struct Native<'a> {
    paths: &'a Paths,
    operation: service::Operation,
    checkpoint: Option<service::Checkpoint>,
}
impl<'a> Native<'a> {
    fn new(paths: &'a Paths) -> Result<Self> {
        Ok(Self {
            paths,
            operation: service::Operation::acquire(paths)?,
            checkpoint: None,
        })
    }
}
impl Control for Native<'_> {
    async fn prepare(&mut self, installation: bool) -> Result<Before> {
        let installed = self.operation.installed()?;
        if installed || installation {
            self.checkpoint = Some(self.operation.checkpoint()?);
        }
        let state = client::presence(self.paths).await?;
        let managed = self
            .checkpoint
            .as_ref()
            .is_some_and(|checkpoint| checkpoint.registration.running);
        Ok(Before {
            running: state != DaemonPresence::Stopped || managed,
            managed,
            installed,
        })
    }
    fn installed(&mut self) -> Result<bool> {
        self.operation.installed()
    }
    fn stop_service(&mut self) -> Result<()> {
        self.operation.stop()
    }
    async fn presence(&mut self) -> Result<DaemonPresence> {
        client::presence(self.paths).await
    }
    async fn shutdown(&mut self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(2), client::request(self.paths, Command::Shutdown))
            .await.map_err(|_| client::ClientError { code:"daemon_unresponsive".into(), message:"the existing daemon did not acknowledge Shutdown; it may still process that request".into() })??;
        Ok(())
    }
    async fn wait_stopped(&mut self) -> Result<()> {
        super::wait_stopped(self.paths).await
    }
    async fn wait_running(&mut self) -> Result<()> {
        client::wait_running(self.paths, Duration::from_secs(5)).await
    }
    fn install(&mut self) -> Result<()> {
        self.operation.install()
    }
    fn refresh(&mut self) -> Result<()> {
        self.operation.refresh(
            self.checkpoint
                .as_ref()
                .context("service restart has no recovery checkpoint")?,
        )
    }
    fn spawn(&mut self) -> Result<()> {
        background::spawn(self.paths)
    }
    fn uninstall(&mut self) -> Result<()> {
        self.operation.uninstall()
    }
    async fn restore(&mut self, before: Before) -> Result<()> {
        if let Some(checkpoint) = &self.checkpoint {
            self.operation.validate_restore(checkpoint)?;
        }
        // A newly launched process must release the owner lock before restoring
        // the old service. Never race recovery against a still-stopping daemon.
        if self.operation.installed()? {
            self.operation.stop()?;
        }
        if client::presence(self.paths).await? != DaemonPresence::Stopped {
            let _ = self.shutdown().await;
        }
        self.wait_stopped().await?;
        if let Some(checkpoint) = &self.checkpoint {
            self.operation.restore(checkpoint)?;
        }
        if before.running {
            if before.managed {
                self.operation.start()?;
            } else {
                background::spawn(self.paths)?;
            }
            self.wait_running().await?;
        }
        Ok(())
    }
}

pub(super) async fn stop(paths: &Paths) -> Result<()> {
    stop_with(&mut Native::new(paths)?).await
}
pub(super) async fn install(paths: &Paths) -> Result<()> {
    install_with(&mut Native::new(paths)?).await
}
pub(super) async fn restart(paths: &Paths) -> Result<()> {
    restart_with(&mut Native::new(paths)?).await
}
pub(super) async fn uninstall(paths: &Paths) -> Result<()> {
    uninstall_with(&mut Native::new(paths)?).await
}

async fn stop_with(control: &mut impl Control) -> Result<()> {
    if control.installed()? {
        control.stop_service()?;
    }
    let state = control.presence().await?;
    let acknowledged = if state != DaemonPresence::Stopped {
        control.shutdown().await
    } else {
        Ok(())
    };
    match control.wait_stopped().await {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Err(shutdown) = acknowledged {
                Err(client::ClientError { code:"daemon_unresponsive".into(), message:client::unresponsive_message(format!("the daemon still owns this profile and did not confirm Shutdown; it is not known to be shutting down. {shutdown:#}; {error:#}")) }.into())
            } else {
                Err(error)
            }
        }
    }
}

async fn failed_install(
    control: &mut impl Control,
    before: Before,
    error: anyhow::Error,
) -> anyhow::Error {
    let failure_code = error
        .downcast_ref::<service::ServiceError>()
        .map(|error| error.code.clone())
        .or_else(|| {
            error
                .downcast_ref::<client::ClientError>()
                .map(|error| error.code.clone())
        })
        .unwrap_or_else(|| "service_install_failed".into());
    match control.restore(before).await {
        Ok(()) => service::ServiceError { code:failure_code, message:format!("service change failed; the previous service definition and {} runtime were restored: {error:#}", if before.running { "running" } else { "stopped" }) }.into(),
        Err(recovery) => service::ServiceError { code:"service_recovery_failed".into(), message:format!("service change failed and the previous runtime could not be fully restored. Check service status and daemon status before retrying. Original failure: {error:#}; recovery failure: {recovery:#}") }.into(),
    }
}

async fn install_with(control: &mut impl Control) -> Result<()> {
    // Capture and validate before stopping a healthy unmanaged daemon.
    let before = control.prepare(true).await?;
    if before.running {
        stop_before_change(control, before).await?;
    }
    let result = async {
        control.install()?;
        control.wait_running().await
    }
    .await;
    if let Err(error) = result {
        return Err(failed_install(control, before, error).await);
    }
    Ok(())
}

async fn restart_with(control: &mut impl Control) -> Result<()> {
    let before = control.prepare(false).await?;
    stop_before_change(control, before).await?;
    let result = async {
        if before.installed {
            control.refresh()?;
        } else {
            control.spawn()?;
        }
        control.wait_running().await
    }
    .await;
    if let Err(error) = result {
        return Err(failed_install(control, before, error).await);
    }
    Ok(())
}

async fn stop_before_change(control: &mut impl Control, before: Before) -> Result<()> {
    if let Err(error) = stop_with(control).await {
        // A manager can report an error after stopping its process. Restore a
        // previously running profile only once the old owner is confirmed gone.
        if before.running && matches!(control.presence().await, Ok(DaemonPresence::Stopped)) {
            return Err(failed_install(control, before, error).await);
        }
        return Err(error.context("replacement was not started because stopping the previous daemon could not be confirmed"));
    }
    Ok(())
}

async fn uninstall_with(control: &mut impl Control) -> Result<()> {
    // The adapter queries OS registration even when the local marker vanished.
    control.uninstall()?;
    if control.presence().await? != DaemonPresence::Stopped {
        let _ = control.shutdown().await;
    }
    control
        .wait_stopped()
        .await
        .context("login startup was removed, but stopping the daemon could not be confirmed")
}

#[cfg(test)]
#[path = "service_lifecycle_tests.rs"]
mod tests;
