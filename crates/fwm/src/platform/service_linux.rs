use super::{
    CommandExecutor, Definition, Registration, ServiceAdapter, ServiceError, discovery, identifier,
    registration_failed, run,
};
use anyhow::{Context, Result, bail};
use fwm_core::paths::Paths;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

pub(super) struct Service {
    paths: Paths,
    targets: Vec<PathBuf>,
    executable: PathBuf,
}
impl Service {
    pub(super) fn new(paths: &Paths, config_home: &Path, executable: PathBuf) -> Self {
        Self {
            paths: paths.clone(),
            targets: vec![
                config_home
                    .join("systemd/user")
                    .join(format!("{}.service", identifier(paths))),
            ],
            executable,
        }
    }
    fn name(target: &Path) -> String {
        target.file_name().unwrap().to_string_lossy().into_owned()
    }
    fn one(&self) -> Result<&Path> {
        if self.targets.len() != 1 {
            return Err(discovery::conflict(
                &self
                    .targets
                    .iter()
                    .map(|path| Self::name(path))
                    .collect::<Vec<_>>(),
            ));
        }
        Ok(&self.targets[0])
    }
    fn inspect(target: &Path, executor: &mut dyn CommandExecutor) -> Result<Registration> {
        let result = executor.execute(Command::new("systemctl").args([
            "--user",
            "show",
            &Self::name(target),
            "--property=LoadState",
            "--property=ActiveState",
            "--property=UnitFileState",
            "--property=NeedDaemonReload",
        ]))?;
        let field = |name: &str| {
            result
                .stdout
                .lines()
                .find_map(|line| line.strip_prefix(&format!("{name}=")))
        };
        if field("LoadState") == Some("not-found") {
            return Ok(Registration::default());
        }
        if !result.success {
            let unavailable = result.stderr.contains("Failed to connect to")
                || result.stderr.contains("not been booted")
                || result.stderr.contains("No medium found");
            return Err(ServiceError::new(
                if unavailable {
                    "service_manager_unavailable"
                } else {
                    "service_query_failed"
                },
                format!("cannot query user systemd service: {}", result.details),
            )
            .into());
        }
        let missing = |name| {
            ServiceError::new(
                "service_query_failed",
                format!("systemctl did not return {name}; service state is unknown"),
            )
        };
        let load = field("LoadState").ok_or_else(|| missing("LoadState"))?;
        let active = field("ActiveState").ok_or_else(|| missing("ActiveState"))?;
        Ok(Registration {
            registered: load != "not-found",
            running: !matches!(active, "inactive" | "failed"),
            enabled: field("UnitFileState").is_some_and(|state| state.starts_with("enabled")),
            // systemd keeps its parsed definition until daemon-reload. Refuse
            // to checkpoint edited/missing disk contents as that loaded state.
            definition: if field("NeedDaemonReload") == Some("no") {
                super::definition::read_optional(target)?
                    .and_then(|bytes| String::from_utf8(bytes).ok())
            } else {
                None
            },
        })
    }
}

impl Service {
    pub(super) fn discover(
        mut self,
        directory: &Path,
        executor: &mut dyn CommandExecutor,
    ) -> Result<Self> {
        let paths = &self.paths;
        let mut targets = discovery::file_candidates(
            directory,
            "fwm-",
            ".service",
            paths,
            discovery::unit_profile,
        )?;
        // Loaded orphaned units can outlive their disk definition. Discover only
        // FWM units and prove the configuration path before adopting a legacy ID.
        if let Some(list) = discovery::query(
            executor,
            Command::new("systemctl").args([
                "--user",
                "list-units",
                "--all",
                "--plain",
                "--no-legend",
                "fwm-*.service",
            ]),
            &["Failed to connect to", "not been booted", "No medium found"],
        )? {
            for line in list.stdout.lines() {
                let Some(name) = line
                    .split_whitespace()
                    .next()
                    .filter(|name| name.starts_with("fwm-") && name.ends_with(".service"))
                else {
                    continue;
                };
                if let Some(details) = discovery::query(
                    executor,
                    Command::new("systemctl").args([
                        "--user",
                        "show",
                        name,
                        "--property=ExecStart",
                        "--value",
                    ]),
                    &[
                        "could not be found",
                        "not found",
                        "Failed to connect to",
                        "not been booted",
                    ],
                )? {
                    let matching = details
                        .stdout
                        .split_once(" --config-dir ")
                        .and_then(|(_, tail)| tail.rsplit_once(" daemon run"))
                        .is_some_and(|(value, _)| {
                            discovery::same_profile(
                                Path::new(value.trim().trim_matches('"')),
                                paths,
                            )
                        });
                    if matching {
                        targets.push(directory.join(name));
                    } else if targets.contains(&directory.join(name))
                        || discovery::legacy_ids(paths)
                            .iter()
                            .any(|id| name == format!("{id}.service"))
                    {
                        return Err(discovery::identity_conflict(name));
                    }
                }
            }
        }
        for name in discovery::legacy_ids(paths) {
            let candidate = directory.join(format!("{name}.service"));
            if candidate.exists() {
                let matching = std::fs::read_to_string(&candidate)
                    .ok()
                    .and_then(|text| discovery::unit_profile(&text))
                    .is_some_and(|profile| discovery::same_profile(&profile, paths));
                if !matching {
                    return Err(discovery::identity_conflict(&Self::name(&candidate)));
                }
                targets.push(candidate);
            }
        }
        targets.sort();
        targets.dedup();
        for target in &targets {
            discovery::validate_profile_file(target, paths, discovery::unit_profile)?;
        }
        if !targets.is_empty() {
            self.targets = targets;
        }
        Ok(self)
    }
}

#[cfg(target_os = "linux")]
pub(super) fn service(paths: &Paths) -> Result<Service> {
    let base = directories::BaseDirs::new().context("cannot find user configuration directory")?;
    Service::new(paths, base.config_dir(), std::env::current_exe()?).discover(
        &base.config_dir().join("systemd/user"),
        &mut super::SystemExecutor,
    )
}

impl ServiceAdapter for Service {
    fn definition_path(&self) -> &Path {
        &self.targets[0]
    }
    fn is_installed(&self) -> bool {
        self.targets.iter().any(|target| target.exists())
    }
    fn render(&self) -> Result<String> {
        self.one()?;
        Ok(format!(
            "[Unit]\nDescription=FWM SSH port forward manager\nAfter=network.target\n\n[Service]\nType=simple\nExecStart={} --config-dir {} daemon run\nRestart=on-failure\nRestartSec=3\nUMask=0077\n\n[Install]\nWantedBy=default.target\n",
            quote_argument(&self.executable.to_string_lossy())?,
            quote_argument(&self.paths.config_dir.to_string_lossy())?
        ))
    }
    fn registration(&self, executor: &mut dyn CommandExecutor) -> Result<Registration> {
        let mut combined = Registration::default();
        for target in &self.targets {
            let registration = Self::inspect(target, executor)?;
            combined.registered |= registration.registered;
            combined.running |= registration.running;
            combined.enabled |= registration.enabled;
            combined.definition = combined.definition.or(registration.definition);
        }
        Ok(combined)
    }
    fn register_saved(&self, executor: &mut dyn CommandExecutor, enabled: bool) -> Result<()> {
        let target = self.one()?;
        run(
            executor,
            Command::new("systemctl").args(["--user", "daemon-reload"]),
        )?;
        run(
            executor,
            Command::new("systemctl").args([
                "--user",
                if enabled { "enable" } else { "disable" },
                &Self::name(target),
            ]),
        )
    }
    fn set_enabled(&self, executor: &mut dyn CommandExecutor, enabled: bool) -> Result<()> {
        run(
            executor,
            Command::new("systemctl").args([
                "--user",
                if enabled { "enable" } else { "disable" },
                &Self::name(self.one()?),
            ]),
        )
    }
    fn install(&self, executor: &mut dyn CommandExecutor) -> Result<()> {
        let target = self.one()?;
        let definition = Definition::stage(target, &self.render()?)?;
        if let Err(error) = self.register_saved(executor, true) {
            return Err(registration_failed(error, definition, |_| {
                run(
                    executor,
                    Command::new("systemctl").args(["--user", "daemon-reload"]),
                )
            }));
        }
        definition.commit();
        self.start(executor)
            .context("login startup is installed, but the service could not start")
    }
    fn unregister(&self, executor: &mut dyn CommandExecutor) -> Result<()> {
        for target in &self.targets {
            let state = Self::inspect(target, executor)?;
            if state.running {
                run(
                    executor,
                    Command::new("systemctl").args(["--user", "stop", &Self::name(target)]),
                )?;
            }
            if state.registered || target.exists() {
                run(
                    executor,
                    Command::new("systemctl").args(["--user", "disable", &Self::name(target)]),
                )?;
            }
        }
        Ok(())
    }
    fn uninstall(&self, executor: &mut dyn CommandExecutor) -> Result<()> {
        self.unregister(executor)?;
        for target in &self.targets {
            super::definition::restore(target, None)?;
        }
        run(
            executor,
            Command::new("systemctl").args(["--user", "daemon-reload"]),
        )
        .context("login definitions were removed, but refreshing the systemd manager failed")
    }
    fn finish_restore(
        &self,
        executor: &mut dyn CommandExecutor,
        previous: &Registration,
    ) -> Result<()> {
        if !previous.registered {
            run(
                executor,
                Command::new("systemctl").args(["--user", "daemon-reload"]),
            )?;
            if !self.definition_path().exists() && self.registration(executor)?.registered {
                return Err(ServiceError::new(
                    "service_recovery_failed",
                    "the removed service remains registered after refreshing systemd",
                )
                .into());
            }
        }
        Ok(())
    }
    fn start(&self, executor: &mut dyn CommandExecutor) -> Result<()> {
        let target = self.one()?;
        let state = Self::inspect(target, executor)?;
        if !state.registered {
            self.register_saved(executor, true)?;
        }
        run(
            executor,
            Command::new("systemctl").args(["--user", "start", &Self::name(target)]),
        )
    }
    fn stop(&self, executor: &mut dyn CommandExecutor) -> Result<()> {
        for target in &self.targets {
            let state = Self::inspect(target, executor)?;
            if state.registered && state.running {
                run(
                    executor,
                    Command::new("systemctl").args(["--user", "stop", &Self::name(target)]),
                )?;
            }
        }
        Ok(())
    }
}

fn quote_argument(value: &str) -> Result<String> {
    if value.contains(['\n', '\r', '\0']) {
        bail!("service paths cannot contain control characters");
    }
    Ok(format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arguments_do_not_expand_unit_specifiers_or_environment() {
        assert_eq!(
            quote_argument("/tmp/a b/%h/$HOME/\"").unwrap(),
            "\"/tmp/a b/%%h/$$HOME/\\\"\""
        );
        assert!(quote_argument("/tmp/a\nRestart=always").is_err());
    }
}
