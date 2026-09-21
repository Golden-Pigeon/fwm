use super::{
    CommandExecutor, Definition, Registration, ServiceAdapter, ServiceError, discovery, identifier,
    registration_failed, run, xml_escape,
};
use anyhow::{Context, Result, bail};
use fwm_core::paths::Paths;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone)]
pub(super) struct Service {
    paths: Paths,
    target: PathBuf,
    executable: PathBuf,
    user_sid: String,
    names: Vec<String>,
}
impl Service {
    pub(super) fn new(paths: &Paths, user_sid: String, executable: PathBuf) -> Self {
        Self {
            paths: paths.clone(),
            target: paths.state_dir.join("login-task.xml"),
            executable,
            user_sid,
            names: vec![],
        }
    }
    fn definition(&self) -> Result<String> {
        let arguments = format!(
            "--config-dir {} daemon run",
            quote_argument(&self.paths.config_dir.to_string_lossy())?
        );
        Ok(format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
<RegistrationInfo><Description>FWM SSH port forward manager</Description></RegistrationInfo>
<Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers>
<Principals><Principal id="Author"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
<Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><StartWhenAvailable>true</StartWhenAvailable><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure></Settings>
<Actions Context="Author"><Exec><Command>{executable}</Command><Arguments>{arguments}</Arguments></Exec></Actions>
</Task>
"#,
            user = xml_escape(&self.user_sid),
            executable = xml_escape(&self.executable.to_string_lossy()),
            arguments = xml_escape(&arguments)
        ))
    }
}

// Query current-user FWM tasks without inserting paths, names or other user
// input into executable PowerShell text. TASK_STATE 2=queued, 4=running.
const QUERY: &str = r#"$ErrorActionPreference='Stop'; [Console]::OutputEncoding=New-Object System.Text.UTF8Encoding($false)
try {
  $scheduler=New-Object -ComObject 'Schedule.Service'; $scheduler.Connect()
  $sid=[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
  $items=@()
  foreach($task in $scheduler.GetFolder('\').GetTasks(1)) {
    if($task.Name.StartsWith('fwm-') -and $task.Definition.Principal.UserId -eq $sid) {
      $items += [pscustomobject]@{name=$task.Name; xml=$task.Xml; state=[int]$task.State; enabled=[bool]$task.Enabled}
    }
  }
  ConvertTo-Json -InputObject $items -Compress -Depth 5
} catch {
  $cause=$_.Exception; while($cause.InnerException) { $cause=$cause.InnerException }
  [Console]::Error.WriteLine($cause.Message)
  $code=$cause.HResult -band 0xffffffffL
  if($code -in @(2147944122L,2147943458L,2147746132L,2147750677L)) { exit 3 }
  exit 4
}"#;

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Task {
    name: String,
    xml: String,
    state: u32,
    enabled: bool,
}
impl Task {
    fn running(&self) -> bool {
        matches!(self.state, 2 | 4)
    }
}

fn query(executor: &mut dyn CommandExecutor) -> Result<Vec<Task>> {
    let result = executor
        .execute(Command::new("powershell.exe").args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            QUERY,
        ]))
        .map_err(|error| {
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            }) {
                ServiceError::new(
                    "service_manager_unavailable",
                    format!("Task Scheduler query requires PowerShell: {error:#}"),
                )
                .into()
            } else {
                error
            }
        })?;
    if !result.success {
        let code = if result.code == Some(3) {
            "service_manager_unavailable"
        } else {
            "service_query_failed"
        };
        let details = if result.stderr.trim().is_empty() {
            result.details.as_str()
        } else {
            result.stderr.trim()
        };
        return Err(ServiceError::new(
            code,
            format!(
                "cannot query current-user Task Scheduler registrations: {}",
                details
            ),
        )
        .into());
    }
    let tasks: Vec<Task> = serde_json::from_str(result.stdout.trim_start_matches('\u{feff}'))
        .map_err(|error| {
            ServiceError::new(
                "service_query_failed",
                format!("invalid Task Scheduler query response: {error}"),
            )
        })?;
    if tasks.iter().any(|task| !(1..=4).contains(&task.state)) {
        return Err(ServiceError::new(
            "service_query_failed",
            "Task Scheduler returned an unknown task state",
        )
        .into());
    }
    Ok(tasks)
}

impl Service {
    fn matching(&self, tasks: Vec<Task>) -> Result<Vec<Task>> {
        let legacy = discovery::legacy_ids(&self.paths);
        let mut matching = vec![];
        for task in tasks {
            if !task.name.starts_with("fwm-") {
                continue;
            }
            let matches = discovery::task_profile(&task.xml)
                .is_some_and(|path| discovery::same_profile(&path, &self.paths));
            if matches {
                matching.push(task);
            } else if legacy.contains(&task.name) || self.names.contains(&task.name) {
                return Err(ServiceError::new("service_identity_conflict", format!("task {:?} does not identify this configuration; refusing to operate on another or malformed task", task.name)).into());
            }
        }
        matching.sort_by(|a, b| a.name.cmp(&b.name));
        matching.dedup_by(|a, b| a.name == b.name);
        Ok(matching)
    }
    fn discover(mut self, executor: &mut dyn CommandExecutor) -> Result<Self> {
        self.names = self
            .matching(query(executor)?)?
            .into_iter()
            .map(|task| task.name)
            .collect();
        Ok(self)
    }
    fn discover_optional(self, executor: &mut dyn CommandExecutor) -> Result<Self> {
        let marker_exists = match std::fs::symlink_metadata(&self.target) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        match self.clone().discover(executor) {
            Ok(service) => Ok(service),
            Err(error) if !marker_exists && super::manager_unavailable(&error) => Ok(self),
            Err(error) => Err(error),
        }
    }
    fn name(&self) -> Result<String> {
        if self.names.len() > 1 {
            return Err(discovery::conflict(&self.names));
        }
        Ok(self
            .names
            .first()
            .cloned()
            .unwrap_or_else(|| identifier(&self.paths)))
    }
    fn current(&self, executor: &mut dyn CommandExecutor) -> Result<Vec<Task>> {
        self.matching(query(executor)?)
    }
}

#[cfg(windows)]
pub(super) fn service(paths: &Paths) -> Result<Service> {
    Service::new(
        paths,
        crate::platform::ipc::current_user_sid()?,
        std::env::current_exe()?,
    )
    .discover_optional(&mut super::SystemExecutor)
}

impl ServiceAdapter for Service {
    fn definition_path(&self) -> &Path {
        &self.target
    }
    fn registration(&self, executor: &mut dyn CommandExecutor) -> Result<Registration> {
        let tasks = self.current(executor)?;
        Ok(Registration {
            registered: !tasks.is_empty(),
            running: tasks.iter().any(Task::running),
            enabled: tasks.iter().any(|task| task.enabled),
            definition: tasks.first().map(|task| task.xml.clone()),
        })
    }
    fn render(&self) -> Result<String> {
        self.name()?;
        self.definition()
    }
    fn register_saved(&self, executor: &mut dyn CommandExecutor, enabled: bool) -> Result<()> {
        let name = self.name()?;
        run(
            executor,
            Command::new("schtasks.exe")
                .args(["/Create", "/TN", &name, "/XML"])
                .arg(&self.target)
                .arg("/F"),
        )?;
        self.set_enabled(executor, enabled)
    }
    fn set_enabled(&self, executor: &mut dyn CommandExecutor, enabled: bool) -> Result<()> {
        let name = self.name()?;
        run(
            executor,
            Command::new("schtasks.exe").args([
                "/Change",
                "/TN",
                &name,
                if enabled { "/ENABLE" } else { "/DISABLE" },
            ]),
        )
    }
    fn install(&self, executor: &mut dyn CommandExecutor) -> Result<()> {
        let rendered = self.render()?;
        let current = self.current(executor)?;
        if current.len() > 1 {
            return Err(discovery::conflict(
                &current
                    .iter()
                    .map(|task| task.name.clone())
                    .collect::<Vec<_>>(),
            ));
        }
        let definition = Definition::stage(&self.target, &rendered)?;
        if let Err(error) = self.register_saved(executor, true) {
            return Err(registration_failed(error, definition, |_| Ok(())));
        }
        definition.commit();
        self.start(executor)
            .context("login startup is installed, but the service could not start")
    }
    fn unregister(&self, executor: &mut dyn CommandExecutor) -> Result<()> {
        self.stop(executor)?;
        for task in self.current(executor)? {
            if let Err(error) = run(
                executor,
                Command::new("schtasks.exe").args(["/Delete", "/TN", &task.name, "/F"]),
            ) && self
                .current(executor)?
                .iter()
                .any(|current| current.name == task.name)
            {
                return Err(error);
            }
        }
        Ok(())
    }
    fn uninstall(&self, executor: &mut dyn CommandExecutor) -> Result<()> {
        self.unregister(executor)?;
        match std::fs::remove_file(&self.target) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
    fn start(&self, executor: &mut dyn CommandExecutor) -> Result<()> {
        let name = self.name()?;
        let tasks = self.current(executor)?;
        let Some(task) = tasks.iter().find(|task| task.name == name) else {
            return Err(ServiceError::new(
                "service_not_registered",
                "login task is not registered; run service install to restore it",
            )
            .into());
        };
        if task.running() {
            return Ok(());
        }
        if task.enabled {
            return run(
                executor,
                Command::new("schtasks.exe").args(["/Run", "/TN", &name]),
            );
        }
        // Task Scheduler rejects /Run on a disabled registration. Enable only
        // around this explicit start, then restore the user's login policy on
        // both success and failure, including an uncertain enablement failure.
        let started = self.set_enabled(executor, true).and_then(|()| {
            run(
                executor,
                Command::new("schtasks.exe").args(["/Run", "/TN", &name]),
            )
        });
        let restored = self.set_enabled(executor, false);
        match (started, restored) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) => Err(error.context("could not start login task; its previous disabled login state was restored")),
            (Ok(()), Err(error)) => Err(error.context("login task started, but its previous disabled login state could not be restored")),
            (Err(error), Err(restore)) => Err(error.context(format!("login task start failed and restoring its previous disabled login state also failed: {restore:#}"))),
        }
    }
    fn stop(&self, executor: &mut dyn CommandExecutor) -> Result<()> {
        for task in self.current(executor)?.into_iter().filter(Task::running) {
            // End preserves the logon trigger. A concurrent transition to ready
            // or absent is already the requested result, not a stop failure.
            if let Err(error) = run(
                executor,
                Command::new("schtasks.exe").args(["/End", "/TN", &task.name]),
            ) && self
                .current(executor)?
                .iter()
                .any(|current| current.name == task.name && current.running())
            {
                return Err(error);
            }
        }
        Ok(())
    }
}

fn quote_argument(value: &str) -> Result<String> {
    if value.contains(['\0', '\r', '\n']) {
        bail!("service paths cannot contain control characters");
    }
    // Standard CommandLineToArgvW quoting: double backslashes before a quote or
    // the closing quote, preserving spaces and a trailing directory separator.
    let mut result = String::from("\"");
    let mut slashes = 0;
    for character in value.chars() {
        if character == '\\' {
            slashes += 1;
            continue;
        }
        if character == '"' {
            result.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
        } else {
            result.extend(std::iter::repeat_n('\\', slashes));
        }
        slashes = 0;
        result.push(character);
    }
    result.extend(std::iter::repeat_n('\\', slashes * 2));
    result.push('"');
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn task_arguments_preserve_spaces_and_trailing_backslashes() {
        assert_eq!(
            quote_argument("C:\\My Config\\").unwrap(),
            "\"C:\\My Config\\\\\""
        );
        assert_eq!(quote_argument("a\"b").unwrap(), "\"a\\\"b\"");
    }
}

#[cfg(test)]
#[path = "service_windows_tests.rs"]
mod registration_tests;
