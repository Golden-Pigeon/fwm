//! Review reproductions: assertions describe the current defect, not a fix.
use super::*;

#[test]
fn audit_linux_failed_first_install_leaves_registered_unit_after_restore() {
    let f = Fixture::new(Platform::Linux);
    let mut operation = f.operation("/new/fwm");
    let checkpoint = operation.checkpoint().unwrap();
    assert!(!checkpoint.registration.registered);
    operation.install().unwrap();
    // Readiness failed; Native::restore stops the newly installed service first.
    operation.stop().unwrap();
    operation.restore(&checkpoint).unwrap();
    assert!(!operation.adapter.definition_path().exists());
    let state = operation.status().unwrap();
    assert!(state.registered);
    assert!(!state.running);
    println!("REPRO Linux: failed first install rollback removed disk file but left loaded registration; installed()={}", operation.installed().unwrap());
}

#[test]
fn audit_checkpoint_confuses_disk_definition_with_registered_program() {
    for platform in [Platform::Macos, Platform::Linux] {
        let f = Fixture::new(platform);
        let old = f.adapter("/actually-running/fwm");
        old.install(&mut f.executor()).unwrap();
        let edited = f.adapter("/edited-on-disk/fwm").render().unwrap();
        fs::write(old.definition_path(), edited.as_bytes()).unwrap();
        assert!(f.machine.lock().unwrap().tasks.values().all(|task| task.program == "/actually-running/fwm"));
        let mut operation = f.operation("/new/fwm");
        let checkpoint = operation.checkpoint().unwrap();
        operation.stop().unwrap();
        operation.install().unwrap();
        // The outer transaction takes this path after readiness failure.
        operation.restore(&checkpoint).unwrap();
        operation.start().unwrap();
        let machine = f.machine.lock().unwrap();
        assert!(machine.tasks.values().all(|task| task.program == "/edited-on-disk/fwm"));
        println!("REPRO {platform:?}: restore returned Ok but changed registered program from /actually-running/fwm to /edited-on-disk/fwm");
    }
}

#[test]
fn audit_unix_legacy_filename_can_adopt_a_different_profile() {
    for platform in [Platform::Macos, Platform::Linux] {
        let f = Fixture::new(platform);
        let old = f.adapter("/old/fwm");
        old.install(&mut f.executor()).unwrap();
        let own = f.paths.config_dir.to_string_lossy().into_owned();
        let foreign = f.directory.path().join("other-profile");
        let foreign = foreign.to_string_lossy().into_owned();
        let body = fs::read_to_string(old.definition_path()).unwrap();
        // An existing fwm-named definition was edited to launch another profile.
        let body = match platform {
            Platform::Macos => body.replace(&xml_escape(&own), &xml_escape(&foreign)),
            Platform::Linux => body.replace(&own.replace('%', "%%").replace('$', "$$"), &foreign),
        };
        fs::write(old.definition_path(), &body).unwrap();
        {
            let mut machine = f.machine.lock().unwrap();
            let task = machine.tasks.values_mut().next().unwrap();
            task.profile = foreign.clone();
            task.body = body;
        }
        let adapter = f.discovered("/new/fwm");
        adapter.uninstall(&mut f.executor()).unwrap();
        assert!(!old.definition_path().exists());
        assert!(f.machine.lock().unwrap().tasks.is_empty());
        println!("REPRO {platform:?}: uninstall for {own} stopped and removed registration for {foreign}");
    }
}
