use super::{
    args::{DaemonCommand, ServiceCommand},
    output,
};
use crate::{client, platform::service};
use anyhow::Result;
use fwm_core::paths::Paths;
use std::time::Duration;

#[path = "service_lifecycle.rs"]
mod service_lifecycle;

pub async fn run(paths: Paths, command: DaemonCommand, json_output: bool) -> Result<()> {
    match command {
        DaemonCommand::Run => crate::daemon::run(paths).await,
        DaemonCommand::Start => {
            client::ensure_running(&paths).await?;
            message("Daemon is running.", json_output)
        }
        DaemonCommand::Stop => {
            stop(&paths).await?;
            message(
                "Daemon stopped; forward running intent is preserved.",
                json_output,
            )
        }
        DaemonCommand::Restart => {
            service_lifecycle::restart(&paths).await?;
            message(
                "Daemon restarted; forward running intent is preserved.",
                json_output,
            )
        }
        DaemonCommand::Status => {
            let presence = client::presence(&paths).await?;
            let (state, running) = match presence {
                client::DaemonPresence::Running => ("running", Some(true)),
                client::DaemonPresence::Stopped => ("stopped", Some(false)),
                client::DaemonPresence::Unresponsive => ("unresponsive", None),
            };
            if json_output {
                output::json_value(
                    &serde_json::json!({"daemon_running":running,"daemon_state":state,"config_dir":paths.config_dir}),
                )
            } else {
                println!("Daemon: {state}");
                Ok(())
            }
        }
    }
}

async fn stop(paths: &Paths) -> Result<()> {
    service_lifecycle::stop(paths).await
}

pub async fn service(paths: &Paths, command: ServiceCommand, json_output: bool) -> Result<()> {
    match command {
        ServiceCommand::Install { .. } => {
            paths.ensure_dirs()?;
            service_lifecycle::install(paths).await?;
            message("Login startup installed for the current user.", json_output)
        }
        ServiceCommand::Uninstall { .. } => {
            service_lifecycle::uninstall(paths).await?;
            message(
                "Login startup removed; this profile's daemon and active forwards are stopped. Saved forward running intent is preserved.",
                json_output,
            )
        }
        ServiceCommand::Status => {
            let state = service::status(paths)?;
            if json_output {
                output::json_value(&state)
            } else {
                println!("Login startup enabled: {}", state["enabled"]);
                println!(
                    "System registration: {}",
                    if state["registered"] == true {
                        "present"
                    } else {
                        "absent"
                    }
                );
                println!("Service process running: {}", state["running"]);
                println!(
                    "Saved definition: {} ({})",
                    state["definition_path"].as_str().unwrap_or("unknown"),
                    if state["definition_exists"] == true {
                        "present"
                    } else {
                        "missing"
                    }
                );
                Ok(())
            }
        }
    }
}

async fn wait_stopped(paths: &Paths) -> Result<()> {
    wait_stopped_for(paths, Duration::from_secs(15)).await
}

async fn wait_stopped_for(paths: &Paths, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while client::presence(paths).await? != client::DaemonPresence::Stopped {
        if tokio::time::Instant::now() >= deadline {
            return Err(service::ServiceError { code:"daemon_stop_timeout".into(), message:format!("timed out waiting for the daemon to release its socket and instance lock; stopped state is unconfirmed. Inspect {}", paths.log_file.display()) }.into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

fn message(message: &str, json_output: bool) -> Result<()> {
    if json_output {
        output::json_value(&serde_json::json!({"ok":true,"message":message}))
    } else {
        println!("{message}");
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn listening_unresponsive_peer_without_lock_is_not_stopped() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(directory.path().to_owned())).unwrap();
        paths.ensure_dirs().unwrap();
        let listener = crate::platform::ipc::bind(&paths).unwrap();
        let peer = tokio::spawn(async move {
            let mut streams = Vec::new();
            loop {
                streams.push(listener.accept().await.unwrap());
            }
        });
        assert!(!paths.lock_file.exists());
        let error = wait_stopped_for(&paths, Duration::from_millis(10))
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<service::ServiceError>().unwrap().code,
            "daemon_stop_timeout"
        );
        peer.abort();
        let _ = peer.await;
        wait_stopped_for(&paths, Duration::from_millis(10))
            .await
            .unwrap();
    }
}
