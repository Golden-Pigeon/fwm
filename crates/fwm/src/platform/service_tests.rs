//! Cross-platform manager protocol tests; no command reaches the real OS manager.
use super::*;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Clone, Copy, Debug)]
enum Platform {
    Macos,
    Linux,
}
#[derive(Clone)]
struct Registered {
    body: String,
    program: String,
    profile: String,
    running: bool,
    enabled: bool,
}
#[derive(Default)]
struct Machine {
    tasks: BTreeMap<String, Registered>,
    disabled: BTreeSet<String>,
    calls: Vec<Vec<String>>,
    fail: VecDeque<String>,
    unavailable: bool,
}
struct FakeExecutor {
    platform: Platform,
    directory: PathBuf,
    machine: Arc<Mutex<Machine>>,
}
fn success(stdout: impl Into<String>) -> CommandResult {
    CommandResult {
        success: true,
        code: Some(0),
        stdout: stdout.into(),
        ..Default::default()
    }
}
fn failure(code: i32, stderr: &str) -> CommandResult {
    CommandResult {
        success: false,
        code: Some(code),
        stderr: stderr.into(),
        details: stderr.into(),
        ..Default::default()
    }
}
impl FakeExecutor {
    fn record(&self, name: &str, body: String) -> Registered {
        let (program, profile) = match self.platform {
            Platform::Macos => (
                discovery::xml_value(
                    body.split_once("<key>ProgramArguments</key>").unwrap().1,
                    "string",
                )
                .unwrap(),
                discovery::plist_profile(&body)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            ),
            Platform::Linux => (
                discovery::quoted(
                    body.lines()
                        .find_map(|line| line.strip_prefix("ExecStart="))
                        .unwrap(),
                )
                .unwrap(),
                discovery::unit_profile(&body)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            ),
        };
        let _ = name;
        Registered {
            body,
            program,
            profile,
            running: false,
            enabled: false,
        }
    }
    fn path(&self, name: &str) -> PathBuf {
        self.directory.join(match self.platform {
            Platform::Macos => format!("{name}.plist"),
            Platform::Linux => name.to_owned(),
        })
    }
}
impl CommandExecutor for FakeExecutor {
    fn execute(&mut self, command: &mut Command) -> Result<CommandResult> {
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let action = &args[match self.platform {
            Platform::Macos => 0,
            Platform::Linux => 1,
        }];
        let mut machine = self.machine.lock().unwrap();
        machine.calls.push(
            std::iter::once(command.get_program().to_string_lossy().into_owned())
                .chain(args.iter().cloned())
                .collect(),
        );
        if machine.unavailable {
            return Ok(failure(
                1,
                match self.platform {
                    Platform::Macos => "Could not find domain",
                    Platform::Linux => "Failed to connect to bus",
                },
            ));
        }
        if machine.fail.front() == Some(action) {
            machine.fail.pop_front();
            return Ok(failure(1, &format!("injected {action} failure")));
        }
        match self.platform {
            Platform::Macos => match action.as_str() {
                "list" => {
                    return Ok(success(
                        machine
                            .tasks
                            .keys()
                            .map(|name| format!("123\t0\t{name}\n"))
                            .collect::<String>(),
                    ));
                }
                "print-disabled" => {
                    return Ok(success(
                        machine
                            .disabled
                            .iter()
                            .map(|name| format!("\"{name}\" => true\n"))
                            .collect::<String>(),
                    ));
                }
                "bootstrap" => {
                    let body = fs::read_to_string(&args[2])?;
                    let name = discovery::xml_value(
                        body.split_once("<key>Label</key>").unwrap().1,
                        "string",
                    )
                    .unwrap();
                    let mut record = self.record(&name, body);
                    record.running = true;
                    record.enabled = !machine.disabled.contains(&name);
                    machine.tasks.insert(name, record);
                }
                action => {
                    let name = args[1].rsplit('/').next().unwrap();
                    match action {
                        "print" => {
                            return Ok(match machine.tasks.get(name) {
                                Some(task) => success(format!(
                                    "program = {}\narguments = {{\n{}\n--config-dir\n{}\ndaemon\nrun\n}}\n{}",
                                    task.program,
                                    task.program,
                                    task.profile,
                                    if task.running { "pid = 123\n" } else { "" }
                                )),
                                None => failure(113, "Could not find service"),
                            });
                        }
                        "bootout" => {
                            machine.tasks.remove(name);
                        }
                        "enable" => {
                            machine.disabled.remove(name);
                        }
                        "disable" => {
                            machine.disabled.insert(name.into());
                        }
                        "kickstart" => {
                            machine.tasks.get_mut(name).unwrap().running = true;
                        }
                        _ => panic!("unexpected launchctl action {action}"),
                    }
                }
            },
            Platform::Linux => {
                match action.as_str() {
                    "list-units" => {
                        return Ok(success(
                            machine
                                .tasks
                                .keys()
                                .map(|name| format!("{name} loaded active running FWM\n"))
                                .collect::<String>(),
                        ));
                    }
                    "daemon-reload" => {
                        if self.directory.exists() {
                            for file in fs::read_dir(&self.directory)? {
                                let file = file?;
                                let name = file.file_name().to_string_lossy().into_owned();
                                if name.starts_with("fwm-")
                                    && name.ends_with(".service")
                                    && file.file_type()?.is_file()
                                {
                                    let mut record =
                                        self.record(&name, fs::read_to_string(file.path())?);
                                    if let Some(old) = machine.tasks.get(&name) {
                                        record.running = old.running;
                                        record.enabled = old.enabled;
                                    }
                                    machine.tasks.insert(name, record);
                                }
                            }
                        }
                        machine
                            .tasks
                            .retain(|name, task| self.path(name).exists() || task.running);
                    }
                    action => {
                        let name = &args[2];
                        if action == "show" {
                            if args.iter().any(|arg| arg == "--property=ExecStart") {
                                return Ok(match machine.tasks.get(name) {
                                    Some(task) => success(format!(
                                        "{{ path={} ; argv[]={0} --config-dir {} daemon run ; }}",
                                        task.program, task.profile
                                    )),
                                    None => success(""),
                                });
                            }
                            if !machine.tasks.contains_key(name) && self.path(name).exists() {
                                let task = self.record(name, fs::read_to_string(self.path(name))?);
                                machine.tasks.insert(name.clone(), task);
                            }
                            return Ok(match machine.tasks.get(name) {
                                Some(task)=>success(format!("LoadState=loaded\nActiveState={}\nUnitFileState={}\nNeedDaemonReload={}\n",if task.running {"active"} else {"inactive"},if task.enabled {"enabled"} else {"disabled"}, if fs::read_to_string(self.path(name)).ok().as_ref() == Some(&task.body) { "no" } else { "yes" })),
                                None=>CommandResult {stdout:"LoadState=not-found\nActiveState=inactive\nUnitFileState=\n".into(),..failure(4,"not found")},
                            });
                        }
                        match action {
                            "enable" => machine.tasks.get_mut(name).unwrap().enabled = true,
                            "disable" => {
                                if let Some(task) = machine.tasks.get_mut(name) {
                                    task.enabled = false;
                                }
                            }
                            "start" => machine.tasks.get_mut(name).unwrap().running = true,
                            "stop" => {
                                if let Some(task) = machine.tasks.get_mut(name) {
                                    task.running = false;
                                }
                            }
                            _ => panic!("unexpected systemctl action {action}"),
                        }
                    }
                }
            }
        }
        Ok(success(""))
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    paths: Paths,
    platform: Platform,
    machine: Arc<Mutex<Machine>>,
}
impl Fixture {
    fn new(platform: Platform) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(directory.path().join("profile &%$ space"))).unwrap();
        Self {
            directory,
            paths,
            platform,
            machine: Arc::new(Mutex::new(Machine::default())),
        }
    }
    fn definition_dir(&self) -> PathBuf {
        self.directory.path().join(match self.platform {
            Platform::Macos => "Library/LaunchAgents",
            Platform::Linux => "systemd/user",
        })
    }
    fn executor(&self) -> FakeExecutor {
        FakeExecutor {
            platform: self.platform,
            directory: self.definition_dir(),
            machine: self.machine.clone(),
        }
    }
    fn adapter(&self, binary: &str) -> Box<dyn ServiceAdapter> {
        match self.platform {
            Platform::Macos => Box::new(macos::Service::new(
                &self.paths,
                self.directory.path(),
                501,
                binary.into(),
            )),
            Platform::Linux => Box::new(linux::Service::new(
                &self.paths,
                self.directory.path(),
                binary.into(),
            )),
        }
    }
    fn discovered(&self, binary: &str) -> Box<dyn ServiceAdapter> {
        match self.platform {
            Platform::Macos => Box::new(
                macos::Service::new(&self.paths, self.directory.path(), 501, binary.into())
                    .discover(&self.definition_dir(), &mut self.executor())
                    .unwrap(),
            ),
            Platform::Linux => Box::new(
                linux::Service::new(&self.paths, self.directory.path(), binary.into())
                    .discover(&self.definition_dir(), &mut self.executor())
                    .unwrap(),
            ),
        }
    }
    fn operation(&self, binary: &str) -> Operation {
        Operation {
            adapter: self.discovered(binary),
            executor: Box::new(self.executor()),
            _lock: operation_lock::OperationLock::acquire(&self.paths).unwrap(),
        }
    }
    fn fail(&self, action: &str) {
        self.machine.lock().unwrap().fail.push_back(action.into());
    }
}

#[test]
fn install_stop_start_uninstall_and_repeated_uninstall_are_consistent() {
    for platform in [Platform::Macos, Platform::Linux] {
        let f = Fixture::new(platform);
        let service = f.adapter("/tmp/fwm <&\"$%");
        let mut exec = f.executor();
        assert!(!service.registration(&mut exec).unwrap().registered);
        service.install(&mut exec).unwrap();
        assert!(service.registration(&mut exec).unwrap().running);
        let body = fs::read_to_string(service.definition_path()).unwrap();
        assert!(body.contains("--config-dir"));
        service.stop(&mut exec).unwrap();
        assert!(!service.registration(&mut exec).unwrap().running);
        assert!(service.is_installed());
        service.stop(&mut exec).unwrap();
        service.start(&mut exec).unwrap();
        assert!(service.registration(&mut exec).unwrap().running);
        service.uninstall(&mut exec).unwrap();
        assert!(!service.is_installed());
        assert!(!service.registration(&mut exec).unwrap().registered);
        service.uninstall(&mut exec).unwrap();
        assert!(f.machine.lock().unwrap().tasks.is_empty());
    }
}

#[test]
fn changed_executable_refreshes_the_registered_definition_and_preserves_enablement() {
    for platform in [Platform::Macos, Platform::Linux] {
        let f = Fixture::new(platform);
        f.adapter("/old/fwm").install(&mut f.executor()).unwrap();
        let mut operation = f.operation("/new/fwm");
        let checkpoint = operation.checkpoint().unwrap();
        operation.stop().unwrap();
        operation.refresh(&checkpoint).unwrap();
        let state = operation.status().unwrap();
        assert!(state.running);
        assert!(state.enabled);
        assert!(
            fs::read_to_string(operation.adapter.definition_path())
                .unwrap()
                .contains("/new/fwm")
        );
        assert!(
            f.machine
                .lock()
                .unwrap()
                .tasks
                .values()
                .all(|task| task.program == "/new/fwm")
        );
    }
}

#[test]
fn outer_checkpoint_recovers_old_definition_and_manager_after_failed_upgrade() {
    for platform in [Platform::Macos, Platform::Linux] {
        for fault in match platform {
            Platform::Macos => vec!["enable", "bootstrap"],
            Platform::Linux => vec!["daemon-reload", "enable", "start"],
        } {
            let f = Fixture::new(platform);
            let old = f.adapter("/old/fwm");
            old.install(&mut f.executor()).unwrap();
            let original = fs::read(old.definition_path()).unwrap();
            let mut operation = f.operation("/new/fwm");
            let checkpoint = operation.checkpoint().unwrap();
            operation.stop().unwrap();
            f.fail(fault);
            assert!(operation.install().is_err());
            operation.restore(&checkpoint).unwrap();
            operation.start().unwrap();
            assert_eq!(
                fs::read(operation.adapter.definition_path()).unwrap(),
                original
            );
            assert!(operation.status().unwrap().running);
            assert!(
                f.machine
                    .lock()
                    .unwrap()
                    .tasks
                    .values()
                    .all(|task| task.program == "/old/fwm")
            );
        }
    }
}

#[test]
fn orphaned_legacy_registration_without_marker_is_found_and_removed() {
    for platform in [Platform::Macos, Platform::Linux] {
        let f = Fixture::new(platform);
        let service = f.adapter("/old/fwm");
        service.install(&mut f.executor()).unwrap();
        let (old_name, record) = f.machine.lock().unwrap().tasks.pop_first().unwrap();
        let legacy = match platform {
            Platform::Macos => "io.fwm.fwm-deadbeefdeadbeef",
            Platform::Linux => "fwm-deadbeefdeadbeef.service",
        };
        f.machine
            .lock()
            .unwrap()
            .tasks
            .insert(legacy.into(), record);
        fs::remove_file(service.definition_path()).unwrap();
        let discovered = f.discovered("/new/fwm");
        assert!(
            discovered
                .registration(&mut f.executor())
                .unwrap()
                .registered
        );
        discovered.uninstall(&mut f.executor()).unwrap();
        assert!(
            f.machine.lock().unwrap().tasks.is_empty(),
            "{platform:?}: {old_name}"
        );
    }
}

#[test]
fn legacy_definition_keeps_its_id_but_updates_to_the_current_executable() {
    for platform in [Platform::Macos, Platform::Linux] {
        let f = Fixture::new(platform);
        let service = f.adapter("/old/fwm");
        service.install(&mut f.executor()).unwrap();
        let (old_name, mut record) = f.machine.lock().unwrap().tasks.pop_first().unwrap();
        let legacy = match platform {
            Platform::Macos => "io.fwm.fwm-deadbeefdeadbeef",
            Platform::Linux => "fwm-deadbeefdeadbeef.service",
        };
        record.body = record.body.replace(&old_name, legacy);
        let target = f.executor().path(legacy);
        fs::rename(service.definition_path(), &target).unwrap();
        fs::write(&target, &record.body).unwrap();
        f.machine
            .lock()
            .unwrap()
            .tasks
            .insert(legacy.into(), record);
        let discovered = f.discovered("/new/fwm");
        assert_eq!(discovered.definition_path(), target);
        discovered.stop(&mut f.executor()).unwrap();
        discovered.install(&mut f.executor()).unwrap();
        let machine = f.machine.lock().unwrap();
        assert_eq!(machine.tasks.len(), 1);
        assert_eq!(machine.tasks[legacy].program, "/new/fwm");
    }
}

#[test]
fn multiple_legacy_registrations_block_install_but_uninstall_cleans_all() {
    for platform in [Platform::Macos, Platform::Linux] {
        let f = Fixture::new(platform);
        let service = f.adapter("/old/fwm");
        service.install(&mut f.executor()).unwrap();
        let old = f
            .machine
            .lock()
            .unwrap()
            .tasks
            .values()
            .next()
            .unwrap()
            .clone();
        let legacy = match platform {
            Platform::Macos => "io.fwm.fwm-deadbeefdeadbeef",
            Platform::Linux => "fwm-deadbeefdeadbeef.service",
        };
        f.machine.lock().unwrap().tasks.insert(legacy.into(), old);
        let discovered = f.discovered("/new/fwm");
        assert_eq!(
            discovered
                .render()
                .unwrap_err()
                .downcast_ref::<ServiceError>()
                .unwrap()
                .code,
            "service_legacy_conflict"
        );
        assert!(discovered.install(&mut f.executor()).is_err());
        assert_eq!(f.machine.lock().unwrap().tasks.len(), 2);
        discovered.uninstall(&mut f.executor()).unwrap();
        assert!(f.machine.lock().unwrap().tasks.is_empty());
    }
}

#[test]
fn query_errors_are_not_reported_as_absent_registrations() {
    for platform in [Platform::Macos, Platform::Linux] {
        let f = Fixture::new(platform);
        f.fail(match platform {
            Platform::Macos => "print",
            Platform::Linux => "show",
        });
        let error = f
            .adapter("/bin/fwm")
            .registration(&mut f.executor())
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ServiceError>().unwrap().code,
            "service_query_failed"
        );
        f.machine.lock().unwrap().unavailable = true;
        assert_eq!(
            f.adapter("/bin/fwm")
                .registration(&mut f.executor())
                .unwrap_err()
                .downcast_ref::<ServiceError>()
                .unwrap()
                .code,
            "service_manager_unavailable"
        );
    }
}

#[test]
fn unavailable_manager_allows_unmanaged_start_only_without_saved_definition() {
    for platform in [Platform::Macos, Platform::Linux] {
        let f = Fixture::new(platform);
        f.machine.lock().unwrap().unavailable = true;
        let mut operation = f.operation("/bin/fwm");
        assert!(!operation.installed().unwrap());
        let target = operation.adapter.definition_path();
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, operation.adapter.render().unwrap()).unwrap();
        assert!(operation.installed().unwrap());
        assert!(
            operation.checkpoint().is_err(),
            "installed service must not silently fall back to unmanaged"
        );
    }
}

#[test]
fn rollback_ownership_check_precedes_manager_changes() {
    let f = Fixture::new(Platform::Macos);
    f.adapter("/old/fwm").install(&mut f.executor()).unwrap();
    let mut operation = f.operation("/new/fwm");
    let checkpoint = operation.checkpoint().unwrap();
    fs::write(
        operation.adapter.definition_path(),
        "external newer definition",
    )
    .unwrap();
    let count = f.machine.lock().unwrap().calls.len();
    assert!(operation.restore(&checkpoint).is_err());
    assert_eq!(f.machine.lock().unwrap().calls.len(), count);
    assert_eq!(
        fs::read_to_string(operation.adapter.definition_path()).unwrap(),
        "external newer definition"
    );
}

#[test]
fn invalid_definition_destination_fails_before_any_manager_mutation() {
    for platform in [Platform::Macos, Platform::Linux] {
        let f = Fixture::new(platform);
        let service = f.adapter("/bin/fwm");
        fs::create_dir_all(service.definition_path()).unwrap();
        assert!(service.install(&mut f.executor()).is_err());
        assert!(f.machine.lock().unwrap().tasks.is_empty());
    }
}

#[test]
fn unrecoverable_orphan_is_rejected_before_stopping_the_old_service() {
    let f = Fixture::new(Platform::Linux);
    let service = f.adapter("/old/fwm");
    service.install(&mut f.executor()).unwrap();
    fs::remove_file(service.definition_path()).unwrap();
    let mut operation = f.operation("/new/fwm");
    let error = operation.checkpoint().err().unwrap();
    assert_eq!(
        error.downcast_ref::<ServiceError>().unwrap().code,
        "service_recovery_unavailable"
    );
    assert!(
        f.machine
            .lock()
            .unwrap()
            .tasks
            .values()
            .all(|task| task.running)
    );
}

#[test]
fn restoring_an_inactive_mac_registration_does_not_wake_the_daemon() {
    let f = Fixture::new(Platform::Macos);
    f.adapter("/old/fwm").install(&mut f.executor()).unwrap();
    for task in f.machine.lock().unwrap().tasks.values_mut() {
        task.running = false;
    }
    let mut operation = f.operation("/new/fwm");
    let checkpoint = operation.checkpoint().unwrap();
    assert!(!checkpoint.registration.running);
    operation.install().unwrap();
    operation.restore(&checkpoint).unwrap();
    assert!(!operation.status().unwrap().running);
    assert!(operation.adapter.is_installed());
}

#[test]
fn rollback_restores_disabled_login_policy_even_when_the_old_job_was_unloaded() {
    let f = Fixture::new(Platform::Macos);
    let old = f.adapter("/old/fwm");
    old.install(&mut f.executor()).unwrap();
    old.stop(&mut f.executor()).unwrap();
    old.set_enabled(&mut f.executor(), false).unwrap();
    let mut operation = f.operation("/new/fwm");
    let checkpoint = operation.checkpoint().unwrap();
    assert!(!checkpoint.registration.registered);
    assert!(!checkpoint.registration.enabled);
    operation.install().unwrap();
    assert!(operation.status().unwrap().enabled);
    operation.restore(&checkpoint).unwrap();
    let restored = operation.status().unwrap();
    assert!(!restored.running);
    assert!(!restored.enabled);
    assert!(operation.adapter.is_installed());
}

#[test]
fn failed_legacy_enumeration_is_not_treated_as_no_registered_service() {
    let mac = Fixture::new(Platform::Macos);
    mac.fail("list");
    let result = macos::Service::new(&mac.paths, mac.directory.path(), 501, "/bin/fwm".into())
        .discover(&mac.definition_dir(), &mut mac.executor());
    assert_eq!(
        result
            .err()
            .unwrap()
            .downcast_ref::<ServiceError>()
            .unwrap()
            .code,
        "service_query_failed"
    );
    let linux = Fixture::new(Platform::Linux);
    linux.fail("list-units");
    let result = linux::Service::new(&linux.paths, linux.directory.path(), "/bin/fwm".into())
        .discover(&linux.definition_dir(), &mut linux.executor());
    assert_eq!(
        result
            .err()
            .unwrap()
            .downcast_ref::<ServiceError>()
            .unwrap()
            .code,
        "service_query_failed"
    );
}

#[test]
fn corrupt_marker_does_not_prevent_removing_a_verified_os_registration() {
    for platform in [Platform::Macos, Platform::Linux] {
        let f = Fixture::new(platform);
        let service = f.adapter("/old/fwm");
        service.install(&mut f.executor()).unwrap();
        fs::write(service.definition_path(), [0xff, 0xfe, 0xfd]).unwrap();
        service.uninstall(&mut f.executor()).unwrap();
        assert!(!service.definition_path().exists());
        assert!(f.machine.lock().unwrap().tasks.is_empty());
    }
}

#[test]
fn missing_systemd_properties_are_a_query_error_not_an_argument_error() {
    struct Missing;
    impl CommandExecutor for Missing {
        fn execute(&mut self, _: &mut Command) -> Result<CommandResult> {
            Ok(success("unrelated=value\n"))
        }
    }
    let f = Fixture::new(Platform::Linux);
    let error = f
        .adapter("/bin/fwm")
        .registration(&mut Missing)
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<ServiceError>().unwrap().code,
        "service_query_failed"
    );
}

#[test]
fn changed_disk_definition_is_not_checkpointed_as_the_loaded_job() {
    for platform in [Platform::Macos, Platform::Linux] {
        let f = Fixture::new(platform);
        let old = f.adapter("/actually-running/fwm");
        old.install(&mut f.executor()).unwrap();
        fs::write(
            old.definition_path(),
            f.adapter("/edited-on-disk/fwm").render().unwrap(),
        )
        .unwrap();
        let mut operation = f.operation("/new/fwm");
        let calls = f.machine.lock().unwrap().calls.len();
        let error = operation.checkpoint().err().unwrap();
        assert_eq!(
            error.downcast_ref::<ServiceError>().unwrap().code,
            "service_recovery_unavailable"
        );
        let machine = f.machine.lock().unwrap();
        assert!(
            machine
                .tasks
                .values()
                .all(|task| task.running && task.program == "/actually-running/fwm")
        );
        assert!(machine.calls[calls..].iter().all(|call| {
            !call
                .iter()
                .any(|argument| ["stop", "bootout", "daemon-reload"].contains(&argument.as_str()))
        }));
    }
}

#[test]
fn foreign_profile_at_legacy_service_name_is_rejected_without_mutations() {
    for platform in [Platform::Macos, Platform::Linux] {
        for (disk_foreign, loaded_foreign) in [(true, true), (true, false), (false, true)] {
            let f = Fixture::new(platform);
            let old = f.adapter("/old/fwm");
            old.install(&mut f.executor()).unwrap();
            let foreign_paths = Paths::new(Some(f.directory.path().join("other-profile"))).unwrap();
            let own = f.paths.config_dir.to_string_lossy();
            let other = foreign_paths.config_dir.to_string_lossy();
            let original = fs::read_to_string(old.definition_path()).unwrap();
            let body = match platform {
                Platform::Macos => original.replace(&xml_escape(&own), &xml_escape(&other)),
                Platform::Linux => original.replace(
                    &own.replace('\\', "\\\\")
                        .replace('%', "%%")
                        .replace('$', "$$"),
                    &other.replace('\\', "\\\\"),
                ),
            };
            assert_ne!(
                body, original,
                "fixture must actually replace the profile path"
            );
            let saved = if disk_foreign { &body } else { &original };
            fs::write(old.definition_path(), saved).unwrap();
            if loaded_foreign {
                let mut machine = f.machine.lock().unwrap();
                let task = machine.tasks.values_mut().next().unwrap();
                task.profile = other.into_owned();
                task.body = body.clone();
            }
            let result: Result<()> = match platform {
                Platform::Macos => {
                    macos::Service::new(&f.paths, f.directory.path(), 501, "/new/fwm".into())
                        .discover(&f.definition_dir(), &mut f.executor())
                        .map(|_| ())
                }
                Platform::Linux => {
                    linux::Service::new(&f.paths, f.directory.path(), "/new/fwm".into())
                        .discover(&f.definition_dir(), &mut f.executor())
                        .map(|_| ())
                }
            };
            assert_eq!(
                result
                    .unwrap_err()
                    .downcast_ref::<ServiceError>()
                    .unwrap()
                    .code,
                "service_identity_conflict"
            );
            assert_eq!(fs::read_to_string(old.definition_path()).unwrap(), *saved);
            assert!(
                f.machine
                    .lock()
                    .unwrap()
                    .tasks
                    .values()
                    .all(|task| task.running)
            );
        }
    }
}

#[test]
fn failed_first_linux_install_removes_loaded_registration_after_restoring_files() {
    let f = Fixture::new(Platform::Linux);
    let mut operation = f.operation("/new/fwm");
    let checkpoint = operation.checkpoint().unwrap();
    operation.install().unwrap();
    operation.stop().unwrap();
    operation.restore(&checkpoint).unwrap();
    assert!(!operation.adapter.definition_path().exists());
    assert!(!operation.status().unwrap().registered);
    assert!(!operation.installed().unwrap());
}
