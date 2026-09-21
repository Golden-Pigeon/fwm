use super::{
    CommandExecutor, Definition, Registration, ServiceAdapter, ServiceError, discovery, identifier,
    registration_failed, run, xml_escape,
};
use anyhow::{Context, Result};
use fwm_core::paths::Paths;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

pub(super) struct Service {
    paths: Paths,
    targets: Vec<(String, PathBuf)>,
    executable: PathBuf,
    domain: String,
}
impl Service {
    pub(super) fn new(paths: &Paths, home: &Path, uid: u32, executable: PathBuf) -> Self {
        let label = format!("io.fwm.{}", identifier(paths));
        Self {
            paths: paths.clone(),
            targets: vec![(
                label.clone(),
                home.join("Library/LaunchAgents")
                    .join(format!("{label}.plist")),
            )],
            executable,
            domain: format!("gui/{uid}"),
        }
    }
    fn one(&self) -> Result<&(String, PathBuf)> {
        if self.targets.len() != 1 {
            return Err(discovery::conflict(
                &self
                    .targets
                    .iter()
                    .map(|(name, _)| name.clone())
                    .collect::<Vec<_>>(),
            ));
        }
        Ok(&self.targets[0])
    }
    fn service_name(&self, label: &str) -> String {
        format!("{}/{label}", self.domain)
    }
    fn inspect(
        &self,
        label: &str,
        path: &Path,
        executor: &mut dyn CommandExecutor,
    ) -> Result<Registration> {
        let result = executor
            .execute(Command::new("launchctl").args(["print", &self.service_name(label)]))?;
        let previous =
            super::definition::read_optional(path)?.and_then(|bytes| String::from_utf8(bytes).ok());
        if !result.success {
            if result.stderr.contains("Could not find service")
                || result.stdout.contains("Could not find service")
            {
                let enabled = path.exists() && !self.disabled(label, executor)?;
                return Ok(Registration {
                    enabled,
                    definition: previous,
                    ..Registration::default()
                });
            }
            let unavailable = result.stderr.contains("Could not find domain")
                || result.stderr.contains("not supported");
            return Err(ServiceError::new(
                if unavailable {
                    "service_manager_unavailable"
                } else {
                    "service_query_failed"
                },
                format!("cannot query launchd service: {}", result.details),
            )
            .into());
        }
        // launchd's loaded program can differ from the plist on disk. Only
        // checkpoint a known FWM definition that agrees with the loaded job;
        // otherwise replacement must stop before touching the running service.
        let loaded = result
            .stdout
            .lines()
            .find_map(|line| line.trim().strip_prefix("program = "))
            .filter(|_| {
                discovery::launch_profile(&result.stdout)
                    .is_some_and(|path| discovery::same_profile(&path, &self.paths))
            })
            .map(|program| definition(&self.paths, program.trim_matches('"'), label));
        let previous = previous.filter(|disk| {
            loaded.as_ref().is_some_and(|loaded| {
                disk == loaded
                    || *disk
                        == loaded.replace(
                            "<string>/dev/null</string>",
                            &format!(
                                "<string>{}</string>",
                                xml_escape(&self.paths.log_file.to_string_lossy())
                            ),
                        )
            })
        });
        Ok(Registration {
            registered: true,
            running: result.stdout.lines().any(|line| {
                line.trim()
                    .strip_prefix("pid = ")
                    .is_some_and(|pid| pid.parse::<u32>().is_ok_and(|pid| pid > 0))
            }),
            enabled: path.exists() && !self.disabled(label, executor)?,
            definition: previous,
        })
    }
    fn disabled(&self, label: &str, executor: &mut dyn CommandExecutor) -> Result<bool> {
        let result =
            executor.execute(Command::new("launchctl").args(["print-disabled", &self.domain]))?;
        if !result.success {
            return Err(ServiceError::new(
                "service_query_failed",
                format!("cannot query launchd enablement: {}", result.details),
            )
            .into());
        }
        Ok(result.stdout.lines().any(|line| {
            line.contains(&format!("\"{label}\""))
                && line
                    .split_once("=>")
                    .is_some_and(|(_, value)| value.trim().trim_end_matches(',') == "true")
        }))
    }
    fn bootstrap(
        &self,
        label: &str,
        target: &Path,
        executor: &mut dyn CommandExecutor,
    ) -> Result<()> {
        let _ = label;
        run(
            executor,
            Command::new("launchctl")
                .arg("bootstrap")
                .arg(&self.domain)
                .arg(target),
        )
        .context("registering login startup")
    }
}

impl Service {
    pub(super) fn discover(
        mut self,
        directory: &Path,
        executor: &mut dyn CommandExecutor,
    ) -> Result<Self> {
        let paths = &self.paths;
        let candidates = discovery::file_candidates(
            directory,
            "io.fwm.fwm-",
            ".plist",
            paths,
            discovery::plist_profile,
        )?;
        let mut targets: Vec<_> = candidates
            .into_iter()
            .map(|path| {
                (
                    path.file_stem().unwrap().to_string_lossy().into_owned(),
                    path,
                )
            })
            .collect();
        if let Some(list) = discovery::query(
            executor,
            Command::new("launchctl").arg("list"),
            &[
                "Could not find domain",
                "Could not get domain",
                "not supported",
            ],
        )? {
            for line in list.stdout.lines() {
                let Some(label) = line
                    .split_whitespace()
                    .last()
                    .filter(|name| name.starts_with("io.fwm.fwm-"))
                else {
                    continue;
                };
                if let Some(state) = discovery::query(
                    executor,
                    Command::new("launchctl").args(["print", &self.service_name(label)]),
                    &[
                        "Could not find service",
                        "Could not find domain",
                        "Could not get domain",
                    ],
                )? {
                    if discovery::launch_profile(&state.stdout)
                        .is_some_and(|path| discovery::same_profile(&path, paths))
                    {
                        targets.push((label.to_owned(), directory.join(format!("{label}.plist"))));
                    } else if targets.iter().any(|(name, _)| name == label)
                        || discovery::legacy_ids(paths)
                            .iter()
                            .any(|id| label == format!("io.fwm.{id}"))
                    {
                        return Err(discovery::identity_conflict(label));
                    }
                }
            }
        }
        for name in discovery::legacy_ids(paths) {
            let label = format!("io.fwm.{name}");
            let path = directory.join(format!("{label}.plist"));
            if path.exists() {
                let matching = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| discovery::plist_profile(&text))
                    .is_some_and(|profile| discovery::same_profile(&profile, paths));
                if !matching {
                    return Err(discovery::identity_conflict(&label));
                }
                targets.push((label, path));
            }
        }
        targets.sort();
        targets.dedup();
        for (_, path) in &targets {
            discovery::validate_profile_file(path, paths, discovery::plist_profile)?;
        }
        if !targets.is_empty() {
            self.targets = targets;
        }
        Ok(self)
    }
}

#[cfg(target_os = "macos")]
pub(super) fn service(paths: &Paths) -> Result<Service> {
    let base = directories::BaseDirs::new().context("cannot find home directory")?;
    // SAFETY: geteuid has no preconditions and is side-effect free.
    Service::new(
        paths,
        base.home_dir(),
        unsafe { libc::geteuid() },
        std::env::current_exe()?,
    )
    .discover(
        &base.home_dir().join("Library/LaunchAgents"),
        &mut super::SystemExecutor,
    )
}

impl ServiceAdapter for Service {
    fn definition_path(&self) -> &Path {
        &self.targets[0].1
    }
    fn is_installed(&self) -> bool {
        self.targets.iter().any(|(_, path)| path.exists())
    }
    fn render(&self) -> Result<String> {
        let (label, _) = self.one()?;
        Ok(definition(
            &self.paths,
            &self.executable.to_string_lossy(),
            label,
        ))
    }
    fn registration(&self, executor: &mut dyn CommandExecutor) -> Result<Registration> {
        let mut combined = Registration::default();
        for (label, path) in &self.targets {
            let state = self.inspect(label, path, executor)?;
            combined.registered |= state.registered;
            combined.running |= state.running;
            combined.enabled |= state.enabled;
            combined.definition = combined.definition.or(state.definition);
        }
        Ok(combined)
    }
    fn register_saved(&self, executor: &mut dyn CommandExecutor, enabled: bool) -> Result<()> {
        let (label, path) = self.one()?;
        self.set_enabled(executor, true)?;
        self.bootstrap(label, path, executor)?;
        if !enabled {
            self.set_enabled(executor, false)?;
        }
        Ok(())
    }
    fn set_enabled(&self, executor: &mut dyn CommandExecutor, enabled: bool) -> Result<()> {
        let (label, _) = self.one()?;
        run(
            executor,
            Command::new("launchctl").args([
                if enabled { "enable" } else { "disable" },
                &self.service_name(label),
            ]),
        )
    }
    fn restore_registration(
        &self,
        executor: &mut dyn CommandExecutor,
        previous: &Registration,
    ) -> Result<()> {
        if previous.registered && previous.running {
            self.register_saved(executor, previous.enabled)
        } else if self.definition_path().exists() {
            // Bootstrapping a RunAtLoad job would wake a previously stopped
            // daemon. Restore its login policy and leave it unloaded instead.
            self.set_enabled(executor, previous.enabled)
        } else {
            Ok(())
        }
    }
    fn install(&self, executor: &mut dyn CommandExecutor) -> Result<()> {
        let (label, target) = self.one()?;
        std::fs::create_dir_all(&self.paths.state_dir)?;
        let before = self.inspect(label, target, executor)?;
        let definition = Definition::stage(target, &self.render()?)?;
        if before.registered {
            run(
                executor,
                Command::new("launchctl").args(["bootout", &self.service_name(label)]),
            )?;
        }
        self.set_enabled(executor, true)?;
        if let Err(error) = self.bootstrap(label, target, executor) {
            return Err(registration_failed(error, definition, |previous| {
                if previous && before.registered {
                    self.bootstrap(label, target, executor)
                } else {
                    Ok(())
                }
            }));
        }
        definition.commit();
        Ok(())
    }
    fn unregister(&self, executor: &mut dyn CommandExecutor) -> Result<()> {
        self.stop(executor)
    }
    fn uninstall(&self, executor: &mut dyn CommandExecutor) -> Result<()> {
        self.unregister(executor)?;
        for (_, target) in &self.targets {
            super::definition::restore(target, None)?;
        }
        Ok(())
    }
    fn start(&self, executor: &mut dyn CommandExecutor) -> Result<()> {
        let (label, path) = self.one()?;
        let state = self.inspect(label, path, executor)?;
        if !state.registered {
            self.register_saved(executor, state.enabled)?;
        }
        run(
            executor,
            Command::new("launchctl").args(["kickstart", &self.service_name(label)]),
        )
    }
    fn stop(&self, executor: &mut dyn CommandExecutor) -> Result<()> {
        for (label, target) in &self.targets {
            if self.inspect(label, target, executor)?.registered {
                run(
                    executor,
                    Command::new("launchctl").args(["bootout", &self.service_name(label)]),
                )?;
            }
        }
        Ok(())
    }
}

fn definition(paths: &Paths, executable: &str, service_label: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{label}</string>
<key>ProgramArguments</key><array><string>{executable}</string><string>--config-dir</string><string>{config}</string><string>daemon</string><string>run</string></array>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
<key>ThrottleInterval</key><integer>5</integer>
<key>StandardOutPath</key><string>/dev/null</string>
<key>StandardErrorPath</key><string>/dev/null</string>
</dict></plist>
"#,
        label = xml_escape(service_label),
        executable = xml_escape(executable),
        config = xml_escape(&paths.config_dir.to_string_lossy()),
    )
}
