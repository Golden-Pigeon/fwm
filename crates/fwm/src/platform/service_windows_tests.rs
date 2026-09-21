//! Task Scheduler behavior through a stateful fake. No native service command
//! is executed on either the host platform or Windows.
use super::super::CommandResult;
use super::*;
use std::{collections::BTreeMap, fs};

#[derive(Default)]
struct Fake {
    tasks: BTreeMap<String, Task>,
    calls: Vec<Vec<String>>,
    fail: Option<&'static str>,
    fail_disable: bool,
    fail_enable: bool,
    query_response: Option<CommandResult>,
    missing_powershell: bool,
}
impl Fake {
    fn calls_for(&self, action: &str) -> usize {
        self.calls
            .iter()
            .filter(|call| call.get(1).is_some_and(|value| value == action))
            .count()
    }
}
impl CommandExecutor for Fake {
    fn execute(&mut self, command: &mut Command) -> Result<CommandResult> {
        let args: Vec<String> = std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        self.calls.push(args.clone());
        if args[0] == "powershell.exe" {
            assert_eq!(args.last().unwrap(), QUERY, "query script must be static");
            if self.missing_powershell {
                return Err(std::io::Error::from(std::io::ErrorKind::NotFound).into());
            }
            return Ok(self.query_response.take().unwrap_or_else(|| CommandResult {
                success: true,
                stdout: serde_json::to_string(&self.tasks.values().collect::<Vec<_>>()).unwrap(),
                ..Default::default()
            }));
        }
        assert_eq!(args[0], "schtasks.exe");
        if self.fail == Some(args[1].as_str())
            || self.fail_disable && args.contains(&"/DISABLE".into())
            || self.fail_enable && args.contains(&"/ENABLE".into())
        {
            return Ok(CommandResult {
                success: false,
                code: Some(1),
                details: "fake denied".into(),
                ..Default::default()
            });
        }
        let name = args[args.iter().position(|arg| arg == "/TN").unwrap() + 1].clone();
        match args[1].as_str() {
            "/Create" => {
                let path = &args[args.iter().position(|arg| arg == "/XML").unwrap() + 1];
                self.tasks.insert(
                    name.clone(),
                    Task {
                        name,
                        xml: fs::read_to_string(path).unwrap(),
                        state: 3,
                        enabled: true,
                    },
                );
            }
            "/Change" => {
                let task = self.tasks.get_mut(&name).unwrap();
                task.enabled = args.contains(&"/ENABLE".into());
                if !task.running() {
                    task.state = if task.enabled { 3 } else { 1 };
                }
            }
            "/Run" => {
                let task = self.tasks.get_mut(&name).unwrap();
                if !task.enabled {
                    return Ok(CommandResult {
                        success: false,
                        code: Some(1),
                        details: "Task Scheduler refuses to run a disabled task".into(),
                        ..Default::default()
                    });
                }
                task.state = 4;
            }
            "/End" => self.tasks.get_mut(&name).unwrap().state = 3,
            "/Delete" => {
                self.tasks.remove(&name);
            }
            action => panic!("unexpected manager action {action}"),
        }
        Ok(CommandResult {
            success: true,
            ..Default::default()
        })
    }
}

fn fixture() -> (tempfile::TempDir, Service) {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(directory.path().join("config space & more"))).unwrap();
    let service = Service::new(
        &paths,
        "S-1-5-21-test".into(),
        directory.path().join("new bin/fwm.exe"),
    );
    (directory, service)
}
fn task(service: &Service, name: &str, state: u32) -> Task {
    Task {
        name: name.into(),
        xml: service.definition().unwrap(),
        state,
        enabled: state != 1,
    }
}
fn code(error: anyhow::Error) -> String {
    error.downcast_ref::<ServiceError>().unwrap().code.clone()
}

#[test]
fn manager_registration_is_authoritative_with_or_without_a_marker() {
    let (_directory, service) = fixture();
    let mut fake = Fake::default();
    let name = identifier(&service.paths);
    fake.tasks.insert(name.clone(), task(&service, &name, 3));
    assert!(!service.is_installed());
    let registration = service.registration(&mut fake).unwrap();
    assert!(registration.registered && registration.enabled && !registration.running);
    service.stop(&mut fake).unwrap();
    assert_eq!(fake.calls_for("/End"), 0);
    service.uninstall(&mut fake).unwrap();
    assert!(fake.tasks.is_empty());
    assert_eq!(fake.calls_for("/Delete"), 1);
    fs::create_dir_all(service.target.parent().unwrap()).unwrap();
    fs::write(&service.target, "orphan marker").unwrap();
    assert!(!service.registration(&mut fake).unwrap().registered);
    service.uninstall(&mut fake).unwrap();
    assert!(!service.target.exists());
    assert_eq!(fake.calls_for("/Delete"), 1);
}

#[test]
fn install_stop_start_uninstall_and_disabled_restore_use_only_the_owned_task() {
    let (_directory, service) = fixture();
    let mut fake = Fake::default();
    service.install(&mut fake).unwrap();
    assert!(service.registration(&mut fake).unwrap().running);
    let definition = fs::read_to_string(&service.target).unwrap();
    assert!(definition.contains("<RunLevel>LeastPrivilege</RunLevel>"));
    assert!(definition.contains("config space &amp; more"));
    service.start(&mut fake).unwrap();
    assert_eq!(fake.calls_for("/Run"), 1);
    service.stop(&mut fake).unwrap();
    service.stop(&mut fake).unwrap();
    assert_eq!(fake.calls_for("/End"), 1);
    assert!(service.target.exists());
    service.register_saved(&mut fake, false).unwrap();
    let state = service.registration(&mut fake).unwrap();
    assert!(state.registered && !state.running && !state.enabled);
    service.uninstall(&mut fake).unwrap();
    service.uninstall(&mut fake).unwrap();
    assert!(fake.tasks.is_empty() && !service.target.exists());
}

#[test]
fn unknown_legacy_task_names_are_found_from_xml_and_preserved_when_upgraded() {
    let (_directory, service) = fixture();
    let mut fake = Fake::default();
    let old = "fwm-unknown-prior-path-hash";
    let mut legacy = task(&service, old, 3);
    let canonical = xml_escape(&service.paths.config_dir.to_string_lossy());
    legacy.xml = legacy.xml.replace(&canonical, &format!("{canonical}/."));
    fake.tasks.insert(old.into(), legacy);
    let service = service.discover(&mut fake).unwrap();
    assert_eq!(service.name().unwrap(), old);
    service.install(&mut fake).unwrap();
    assert_eq!(fake.tasks.len(), 1);
    assert!(fake.tasks[old].xml.contains("new bin/fwm.exe"));
    assert!(!fake.tasks[old].xml.contains(&format!("{canonical}/.")));
}

#[test]
fn conflicting_legacy_registrations_cannot_install_but_are_all_stopped_and_uninstalled() {
    let (_directory, service) = fixture();
    let mut fake = Fake::default();
    for (name, state) in [("fwm-legacy-a", 4), ("fwm-legacy-b", 2)] {
        fake.tasks.insert(name.into(), task(&service, name, state));
    }
    let service = service.discover(&mut fake).unwrap();
    assert_eq!(
        code(service.render().unwrap_err()),
        "service_legacy_conflict"
    );
    assert_eq!(
        code(service.install(&mut fake).unwrap_err()),
        "service_legacy_conflict"
    );
    assert!(!service.target.exists());
    assert_eq!(fake.calls_for("/Create"), 0);
    service.uninstall(&mut fake).unwrap();
    assert!(fake.tasks.is_empty());
    assert_eq!(fake.calls_for("/End"), 2);
    assert_eq!(fake.calls_for("/Delete"), 2);
}

#[test]
fn query_failure_or_malformed_data_never_masquerades_as_an_absent_task() {
    let (_directory, service) = fixture();
    for (result, expected) in [
        (
            CommandResult {
                success: false,
                code: Some(3),
                details: "RPC unavailable".into(),
                ..Default::default()
            },
            "service_manager_unavailable",
        ),
        (
            CommandResult {
                success: false,
                code: Some(4),
                details: "permission denied".into(),
                ..Default::default()
            },
            "service_query_failed",
        ),
        (
            CommandResult {
                success: true,
                stdout: "not JSON".into(),
                ..Default::default()
            },
            "service_query_failed",
        ),
        (
            CommandResult {
                success: true,
                stdout: "{}".into(),
                ..Default::default()
            },
            "service_query_failed",
        ),
    ] {
        let mut fake = Fake {
            query_response: Some(result),
            ..Default::default()
        };
        assert_eq!(code(service.registration(&mut fake).unwrap_err()), expected);
        assert_eq!(fake.calls.len(), 1);
    }
    let mut fake = Fake {
        missing_powershell: true,
        ..Default::default()
    };
    assert_eq!(
        code(service.registration(&mut fake).unwrap_err()),
        "service_manager_unavailable"
    );
    let mut denied = Fake {
        query_response: Some(CommandResult {
            success: false,
            code: Some(4),
            stderr: "COM access denied".into(),
            details: "generic exit status".into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(
        service
            .registration(&mut denied)
            .unwrap_err()
            .to_string()
            .contains("COM access denied")
    );
    let name = identifier(&service.paths);
    let mut fake = Fake::default();
    fake.tasks.insert(name.clone(), task(&service, &name, 0));
    assert_eq!(
        code(service.registration(&mut fake).unwrap_err()),
        "service_query_failed"
    );
}

#[test]
fn missing_manager_only_allows_factory_fallback_when_no_definition_exists() {
    let (_directory, service) = fixture();
    let mut fake = Fake {
        missing_powershell: true,
        ..Default::default()
    };
    let fallback = service.clone().discover_optional(&mut fake).unwrap();
    assert_eq!(
        code(fallback.registration(&mut fake).unwrap_err()),
        "service_manager_unavailable"
    );
    fs::create_dir_all(service.target.parent().unwrap()).unwrap();
    fs::write(&service.target, "existing task definition").unwrap();
    assert_eq!(
        code(service.clone().discover_optional(&mut fake).err().unwrap()),
        "service_manager_unavailable"
    );
    fs::remove_file(&service.target).unwrap();
    let mut denied = Fake {
        query_response: Some(CommandResult {
            success: false,
            code: Some(4),
            details: "permission denied".into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        code(service.discover_optional(&mut denied).err().unwrap()),
        "service_query_failed"
    );
}

#[test]
fn changing_enabled_state_does_not_recreate_or_restart_a_registered_task() {
    let (_directory, service) = fixture();
    let mut fake = Fake::default();
    let name = identifier(&service.paths);
    let original = task(&service, &name, 4);
    fake.tasks.insert(name.clone(), original.clone());
    service.set_enabled(&mut fake, false).unwrap();
    let state = service.registration(&mut fake).unwrap();
    assert!(state.registered && state.running && !state.enabled);
    service.set_enabled(&mut fake, true).unwrap();
    assert_eq!(fake.tasks[&name].xml, original.xml);
    assert_eq!(fake.calls_for("/Create"), 0);
    assert_eq!(fake.calls_for("/Run"), 0);
    assert_eq!(fake.calls_for("/Change"), 2);
    assert!(!service.target.exists());
}

#[test]
fn explicit_start_and_rollback_can_run_disabled_tasks_without_enabling_future_login() {
    let (_directory, service) = fixture();
    let mut fake = Fake::default();
    let name = identifier(&service.paths);
    fake.tasks.insert(name.clone(), task(&service, &name, 1));
    let unrelated = "fwm-unrelated-user-task";
    fake.tasks.insert(
        unrelated.into(),
        Task {
            name: unrelated.into(),
            xml: "<Arguments>another application</Arguments>".into(),
            state: 3,
            enabled: true,
        },
    );
    service.start(&mut fake).unwrap();
    assert!(fake.tasks[&name].running());
    assert!(!fake.tasks[&name].enabled);
    assert_eq!(fake.tasks[unrelated].state, 3);
    assert!(fake.tasks[unrelated].enabled);
    let actions: Vec<_> = fake
        .calls
        .iter()
        .filter(|call| call[0] == "schtasks.exe")
        .map(|call| (call[1].as_str(), call.last().unwrap().as_str()))
        .collect();
    assert_eq!(
        actions,
        [
            ("/Change", "/ENABLE"),
            ("/Run", name.as_str()),
            ("/Change", "/DISABLE")
        ]
    );
    let calls = fake.calls.len();
    service.start(&mut fake).unwrap();
    assert_eq!(
        fake.calls.len(),
        calls + 1,
        "already-running disabled task needs only a query"
    );
    fs::create_dir_all(service.target.parent().unwrap()).unwrap();
    fs::write(&service.target, service.definition().unwrap()).unwrap();
    // Outer rollback restores a disabled registration before restoring runtime.
    service.register_saved(&mut fake, false).unwrap();
    assert!(!fake.tasks[&name].running() && !fake.tasks[&name].enabled);
    service.start(&mut fake).unwrap();
    assert!(fake.tasks[&name].running() && !fake.tasks[&name].enabled);
}

#[test]
fn disabled_task_start_failures_restore_enablement_and_report_combined_restore_failures() {
    for (fail_run, fail_enable, fail_disable) in [
        (true, false, false),
        (false, true, false),
        (true, false, true),
        (false, true, true),
        (false, false, true),
    ] {
        let (_directory, service) = fixture();
        let mut fake = Fake {
            fail: fail_run.then_some("/Run"),
            fail_enable,
            fail_disable,
            ..Default::default()
        };
        let name = identifier(&service.paths);
        fake.tasks.insert(name.clone(), task(&service, &name, 1));
        let error = service.start(&mut fake).unwrap_err();
        let message = format!("{error:#}");
        assert_eq!(code(error), "service_command_failed");
        assert!(
            fake.calls
                .iter()
                .any(|call| call.contains(&"/DISABLE".into())),
            "restore must be attempted"
        );
        if !fail_disable {
            assert!(!fake.tasks[&name].enabled);
            assert!(message.contains("was restored"));
        } else if fail_run || fail_enable {
            assert!(
                message.contains("start failed")
                    && message.contains("also failed")
                    && message.contains(if fail_run { "/Run" } else { "/ENABLE" })
                    && message.contains("/DISABLE")
            );
        } else {
            assert!(fake.tasks[&name].running());
            assert!(message.contains("started, but") && message.contains("could not be restored"));
        }
    }
}

#[test]
fn task_identity_conflicts_and_manager_mutation_failures_preserve_the_marker() {
    let (_directory, service) = fixture();
    let mut fake = Fake::default();
    let name = identifier(&service.paths);
    fake.tasks.insert(
        name.clone(),
        Task {
            name: name.clone(),
            xml: "<Arguments>other command</Arguments>".into(),
            state: 3,
            enabled: true,
        },
    );
    assert_eq!(
        code(service.uninstall(&mut fake).unwrap_err()),
        "service_identity_conflict"
    );
    assert_eq!(fake.calls_for("/Delete"), 0);
    fake.tasks.insert(name.clone(), task(&service, &name, 3));
    fs::create_dir_all(service.target.parent().unwrap()).unwrap();
    fs::write(&service.target, service.definition().unwrap()).unwrap();
    fake.fail = Some("/Delete");
    assert_eq!(
        code(service.uninstall(&mut fake).unwrap_err()),
        "service_command_failed"
    );
    assert!(service.target.exists() && fake.tasks.contains_key(&name));
    fake.tasks.get_mut(&name).unwrap().state = 4;
    fake.fail = Some("/End");
    assert_eq!(
        code(service.stop(&mut fake).unwrap_err()),
        "service_command_failed"
    );
    assert!(service.target.exists());
}
