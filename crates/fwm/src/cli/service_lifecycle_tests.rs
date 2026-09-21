use super::*;
use std::collections::VecDeque;

struct Fake {
    before: Before,
    installed: bool,
    states: VecDeque<DaemonPresence>,
    failures: Vec<&'static str>,
    failure_code: Option<&'static str>,
    calls: Vec<&'static str>,
}
impl Fake {
    fn new(installed: bool, running: bool, managed: bool) -> Self {
        Self {
            before: Before {
                running,
                managed,
                installed,
            },
            installed,
            states: vec![if running {
                DaemonPresence::Running
            } else {
                DaemonPresence::Stopped
            }]
            .into(),
            failures: vec![],
            failure_code: None,
            calls: vec![],
        }
    }
    fn call(&mut self, operation: &'static str) -> Result<()> {
        self.calls.push(operation);
        if self.failures.contains(&operation) {
            if let Some(code) = self.failure_code {
                return Err(service::ServiceError {
                    code: code.into(),
                    message: format!("injected {operation} failure"),
                }
                .into());
            }
            anyhow::bail!("injected {operation} failure");
        }
        Ok(())
    }
}
impl Control for Fake {
    async fn prepare(&mut self, _: bool) -> Result<Before> {
        self.call("prepare")?;
        Ok(self.before)
    }
    fn installed(&mut self) -> Result<bool> {
        self.call("installed")?;
        Ok(self.installed)
    }
    fn stop_service(&mut self) -> Result<()> {
        self.call("stop_service")
    }
    async fn presence(&mut self) -> Result<DaemonPresence> {
        self.call("presence")?;
        Ok(self.states.pop_front().unwrap_or(DaemonPresence::Stopped))
    }
    async fn shutdown(&mut self) -> Result<()> {
        self.call("shutdown")
    }
    async fn wait_stopped(&mut self) -> Result<()> {
        self.call("wait_stopped")
    }
    async fn wait_running(&mut self) -> Result<()> {
        self.call("wait_running")
    }
    fn install(&mut self) -> Result<()> {
        self.call("install")
    }
    fn refresh(&mut self) -> Result<()> {
        self.call("refresh")
    }
    fn spawn(&mut self) -> Result<()> {
        self.call("spawn")
    }
    async fn restore(&mut self, _: Before) -> Result<()> {
        self.call("restore")
    }
    fn uninstall(&mut self) -> Result<()> {
        self.call("uninstall")
    }
}

#[tokio::test]
async fn installed_service_is_preflighted_then_fully_stopped_before_install() {
    let mut control = Fake::new(true, true, true);
    install_with(&mut control).await.unwrap();
    assert_eq!(
        control.calls,
        [
            "prepare",
            "installed",
            "stop_service",
            "presence",
            "shutdown",
            "wait_stopped",
            "install",
            "wait_running"
        ]
    );
}

#[tokio::test]
async fn unmanaged_daemon_is_handed_over_without_service_stop() {
    let mut control = Fake::new(false, true, false);
    install_with(&mut control).await.unwrap();
    assert_eq!(
        control.calls,
        [
            "prepare",
            "installed",
            "presence",
            "shutdown",
            "wait_stopped",
            "install",
            "wait_running"
        ]
    );
}

#[tokio::test]
async fn installation_preflight_failure_never_stops_the_existing_daemon() {
    let mut control = Fake::new(true, true, true);
    control.failures.push("prepare");
    assert!(install_with(&mut control).await.is_err());
    assert_eq!(control.calls, ["prepare"]);
}

#[tokio::test]
async fn failed_registration_or_readiness_restores_previous_runtime() {
    for failure in ["install", "wait_running"] {
        for managed in [false, true] {
            let mut control = Fake::new(managed, true, managed);
            control.failures.push(failure);
            let error = install_with(&mut control).await.unwrap_err();
            assert_eq!(control.calls.last(), Some(&"restore"));
            assert_eq!(
                error.downcast_ref::<service::ServiceError>().unwrap().code,
                "service_install_failed"
            );
            assert!(error.to_string().contains("running runtime were restored"));
        }
    }
}

#[tokio::test]
async fn failed_recovery_reports_both_failures_without_claiming_rollback() {
    let mut control = Fake::new(true, true, true);
    control.failures = vec!["install", "restore"];
    let error = install_with(&mut control).await.unwrap_err();
    assert_eq!(
        error.downcast_ref::<service::ServiceError>().unwrap().code,
        "service_recovery_failed"
    );
    assert!(error.to_string().contains("injected install"));
    assert!(error.to_string().contains("injected restore"));
}

#[tokio::test]
async fn stop_failure_does_not_attempt_install_or_spawn() {
    for stage in ["installed", "stop_service", "wait_stopped"] {
        let mut control = Fake::new(true, true, true);
        control.failures.push(stage);
        assert!(install_with(&mut control).await.is_err());
        assert!(!control.calls.contains(&"install"));
        assert!(!control.calls.contains(&"spawn"));
    }
}

#[tokio::test]
async fn restart_refreshes_the_managed_executable_before_waiting_for_readiness() {
    let mut control = Fake::new(true, true, true);
    restart_with(&mut control).await.unwrap();
    assert_eq!(
        &control.calls[control.calls.len() - 2..],
        ["refresh", "wait_running"]
    );
    assert!(!control.calls.contains(&"spawn"));
    let mut unmanaged = Fake::new(false, false, false);
    restart_with(&mut unmanaged).await.unwrap();
    assert_eq!(
        &unmanaged.calls[unmanaged.calls.len() - 2..],
        ["spawn", "wait_running"]
    );
}

#[tokio::test]
async fn unresponsive_owner_receives_shutdown_and_unconfirmed_stop_is_explicit() {
    let mut control = Fake::new(false, true, false);
    control.states = vec![DaemonPresence::Unresponsive].into();
    control.failures = vec!["shutdown", "wait_stopped"];
    let error = stop_with(&mut control).await.unwrap_err();
    assert!(control.calls.contains(&"shutdown"));
    assert_eq!(
        error.downcast_ref::<client::ClientError>().unwrap().code,
        "daemon_unresponsive"
    );
    assert!(error.to_string().contains("not known to be shutting down"));
}

#[tokio::test]
async fn lost_shutdown_reply_is_success_if_owner_really_exited() {
    let mut control = Fake::new(false, true, false);
    control.failures.push("shutdown");
    stop_with(&mut control).await.unwrap();
    assert_eq!(control.calls.last(), Some(&"wait_stopped"));
}

#[tokio::test]
async fn uninstall_always_checks_registration_and_waits_for_daemon_release() {
    let mut control = Fake::new(false, true, false);
    uninstall_with(&mut control).await.unwrap();
    assert_eq!(
        control.calls,
        ["uninstall", "presence", "shutdown", "wait_stopped"]
    );
}

#[tokio::test]
async fn stop_error_after_the_owner_exited_restores_previously_running_state() {
    let mut control = Fake::new(true, true, true);
    control.failures.push("stop_service");
    control.states = vec![DaemonPresence::Stopped].into();
    let error = install_with(&mut control).await.unwrap_err();
    assert_eq!(control.calls.last(), Some(&"restore"));
    assert!(!control.calls.contains(&"install"));
    assert_eq!(
        error.downcast_ref::<service::ServiceError>().unwrap().code,
        "service_install_failed"
    );
}

#[tokio::test]
async fn successful_recovery_preserves_the_original_timeout_error_code() {
    let mut control = Fake::new(true, true, true);
    control.failures.push("install");
    control.failure_code = Some("service_timeout");
    let error = install_with(&mut control).await.unwrap_err();
    assert_eq!(
        error.downcast_ref::<service::ServiceError>().unwrap().code,
        "service_timeout"
    );
    assert!(error.to_string().contains("runtime were restored"));
}
