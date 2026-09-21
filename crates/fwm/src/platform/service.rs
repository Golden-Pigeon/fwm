//! Login services use argument arrays and per-profile definitions. Windows uses
//! a fixed, read-only PowerShell COM query with no interpolated user input.
use anyhow::Result;
use fwm_core::paths::Paths;
use std::path::Path;
use std::process::Command;

#[derive(Debug)]
pub(crate) struct ServiceError {
    pub code: String,
    pub message: String,
}
impl ServiceError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}
impl std::fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for ServiceError {}

#[cfg(any(target_os = "linux", test))]
#[path = "service_linux.rs"]
mod linux;
#[cfg(any(target_os = "macos", test))]
#[path = "service_macos.rs"]
mod macos;
#[cfg(any(windows, test))]
#[path = "service_windows.rs"]
mod windows;

#[cfg(target_os = "linux")]
use linux as implementation;
#[cfg(target_os = "macos")]
use macos as implementation;
#[cfg(windows)]
use windows as implementation;

trait ServiceAdapter: Send {
    fn definition_path(&self) -> &Path;
    fn is_installed(&self) -> bool {
        self.definition_path().exists()
    }
    fn install(&self, executor: &mut dyn CommandExecutor) -> Result<()>;
    fn uninstall(&self, executor: &mut dyn CommandExecutor) -> Result<()>;
    fn start(&self, executor: &mut dyn CommandExecutor) -> Result<()>;
    fn stop(&self, executor: &mut dyn CommandExecutor) -> Result<()>;
    fn registration(&self, executor: &mut dyn CommandExecutor) -> Result<Registration>;
    fn render(&self) -> Result<String>;
    fn register_saved(&self, executor: &mut dyn CommandExecutor, enabled: bool) -> Result<()>;
    fn restore_registration(
        &self,
        executor: &mut dyn CommandExecutor,
        previous: &Registration,
    ) -> Result<()> {
        if previous.registered {
            self.register_saved(executor, previous.enabled)
        } else {
            Ok(())
        }
    }
    fn unregister(&self, executor: &mut dyn CommandExecutor) -> Result<()>;
    fn set_enabled(&self, executor: &mut dyn CommandExecutor, enabled: bool) -> Result<()>;
    fn finish_restore(&self, _: &mut dyn CommandExecutor, _: &Registration) -> Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub(crate) struct Registration {
    pub registered: bool,
    pub running: bool,
    pub enabled: bool,
    pub definition: Option<String>,
}

#[derive(Clone)]
pub(crate) struct Checkpoint {
    pub registration: Registration,
    definition: Option<Vec<u8>>,
    recovery_definition: Option<Vec<u8>>,
}

#[path = "service_lock.rs"]
mod operation_lock;

/// Holds one profile's service/lifecycle lock for the whole CLI transaction.
pub(crate) struct Operation {
    adapter: Box<dyn ServiceAdapter>,
    executor: Box<dyn CommandExecutor>,
    _lock: operation_lock::OperationLock,
}

impl Operation {
    pub fn acquire(paths: &Paths) -> Result<Self> {
        let lock = operation_lock::OperationLock::acquire(paths)?;
        Ok(Self {
            adapter: Box::new(implementation::service(paths)?),
            executor: Box::new(SystemExecutor),
            _lock: lock,
        })
    }
    pub fn status(&mut self) -> Result<Registration> {
        self.adapter.registration(self.executor.as_mut())
    }
    pub fn installed(&mut self) -> Result<bool> {
        if self.adapter.is_installed() {
            return Ok(true);
        }
        match self.status() {
            Ok(state) => Ok(state.registered),
            Err(error) if manager_unavailable(&error) => Ok(false),
            Err(error) => Err(error),
        }
    }
    pub fn checkpoint(&mut self) -> Result<Checkpoint> {
        let registration = self.status()?;
        let definition = definition::read_optional(self.adapter.definition_path())?;
        // Disk contents are not a substitute for the manager's loaded state.
        let recovery_definition = if registration.registered {
            registration
                .definition
                .as_ref()
                .map(|text| text.as_bytes().to_vec())
        } else {
            definition.clone()
        };
        if registration.registered
            && recovery_definition
                .as_ref()
                .is_none_or(|bytes| std::str::from_utf8(bytes).is_err())
        {
            return Err(ServiceError::new("service_recovery_unavailable", "the manager has this service registered but its loaded definition cannot be confirmed from the saved definition (it may have changed or disappeared). No daemon was stopped. Restore the matching saved definition before retrying, or explicitly use service uninstall then service install to recreate it").into());
        }
        self.adapter.render()?;
        Ok(Checkpoint {
            registration,
            definition,
            recovery_definition,
        })
    }
    pub fn install(&mut self) -> Result<()> {
        self.adapter.install(self.executor.as_mut())
    }
    pub fn start(&mut self) -> Result<()> {
        self.adapter.start(self.executor.as_mut())
    }
    pub fn stop(&mut self) -> Result<()> {
        self.adapter.stop(self.executor.as_mut())
    }
    pub fn uninstall(&mut self) -> Result<()> {
        self.adapter.uninstall(self.executor.as_mut())
    }
    pub fn restore(&mut self, checkpoint: &Checkpoint) -> Result<()> {
        self.validate_restore(checkpoint)?;
        self.adapter.unregister(self.executor.as_mut())?;
        definition::restore(
            self.adapter.definition_path(),
            checkpoint.recovery_definition.as_deref(),
        )?;
        self.adapter
            .restore_registration(self.executor.as_mut(), &checkpoint.registration)?;
        definition::restore(
            self.adapter.definition_path(),
            checkpoint.definition.as_deref(),
        )?;
        self.adapter
            .finish_restore(self.executor.as_mut(), &checkpoint.registration)
    }
    pub fn validate_restore(&self, checkpoint: &Checkpoint) -> Result<()> {
        let current = definition::read_optional(self.adapter.definition_path())?;
        if current != checkpoint.definition
            && current.as_deref() != Some(self.adapter.render()?.as_bytes())
        {
            return Err(ServiceError::new("service_definition_conflict", "service definition changed outside this transaction; recovery did not overwrite it").into());
        }
        Ok(())
    }
    pub fn refresh(&mut self, checkpoint: &Checkpoint) -> Result<()> {
        self.install()?;
        self.adapter
            .set_enabled(self.executor.as_mut(), checkpoint.registration.enabled)
    }
}

#[derive(Default)]
struct CommandResult {
    success: bool,
    details: String,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Only this boundary executes external programs. Tests use an in-memory script.
trait CommandExecutor: Send {
    fn execute(&mut self, command: &mut Command) -> Result<CommandResult>;
}

#[path = "service_command.rs"]
mod command;
use command::SystemExecutor;

pub(crate) fn status(paths: &Paths) -> Result<serde_json::Value> {
    let adapter = implementation::service(paths)?;
    let registration = adapter.registration(&mut SystemExecutor)?;
    Ok(
        serde_json::json!({"definition_exists":adapter.is_installed(),"definition_path":adapter.definition_path(),"registered":registration.registered,"running":registration.running,"enabled":registration.enabled}),
    )
}

fn identifier(paths: &Paths) -> String {
    identifier_path(&paths.config_dir)
}
fn identifier_path(path: &Path) -> String {
    // Fixed algorithm keeps service names stable between builds and processes.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in path.to_string_lossy().as_bytes() {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
    }
    format!("fwm-{hash:016x}")
}

#[path = "service_discovery.rs"]
mod discovery;

fn manager_unavailable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ServiceError>()
        .is_some_and(|error| error.code == "service_manager_unavailable")
        || error.chain().any(|error| {
            error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        })
}

fn run(executor: &mut dyn CommandExecutor, command: &mut Command) -> Result<()> {
    let result = executor.execute(command)?;
    if !result.success {
        return Err(ServiceError::new(
            "service_command_failed",
            format!(
                "service command failed (exit {:?}, {}): {command:?}",
                result.code, result.details
            ),
        )
        .into());
    }
    Ok(())
}

#[path = "service_definition.rs"]
mod definition;
use definition::Definition;

/// Restore disk state before asking the manager to reload the old definition.
fn registration_failed(
    error: anyhow::Error,
    mut definition: Definition,
    restore_manager: impl FnOnce(bool) -> Result<()>,
) -> anyhow::Error {
    let previous = definition.had_previous();
    let recovery = definition
        .rollback()
        .and_then(|()| restore_manager(previous));
    match recovery {
        Ok(()) => error.context("service registration failed; previous definition was restored"),
        Err(rollback) => error.context(format!(
            "service registration failed; rollback also failed: {rollback:#}"
        )),
    }
}
#[cfg(any(target_os = "macos", windows, test))]
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_registration_restores_previous_definition() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("service.conf");
        {
            let _new = Definition::stage(&path, "new").unwrap();
        }
        assert!(!path.exists());
        std::fs::write(&path, "old").unwrap();
        {
            let _replacement = Definition::stage(&path, "new").unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
        Definition::stage(&path, "registered").unwrap().commit();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "registered");
    }
    #[test]
    fn xml_values_cannot_insert_extra_arguments() {
        assert_eq!(
            xml_escape("a</string><string>&\"'"),
            "a&lt;/string&gt;&lt;string&gt;&amp;&quot;&apos;"
        );
    }
    #[test]
    fn profile_services_are_distinct_and_stable() {
        let a = Paths::new(Some(std::env::temp_dir().join("fwm-service-a"))).unwrap();
        let b = Paths::new(Some(std::env::temp_dir().join("fwm-service-b"))).unwrap();
        assert_eq!(identifier(&a), identifier(&a));
        assert_ne!(identifier(&a), identifier(&b));
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod workflow_tests;
