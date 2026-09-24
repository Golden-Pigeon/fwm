use fwm_core::paths::{Paths, absolute_normalized};
use std::{fs, path::PathBuf};

#[test]
fn default_and_explicit_home_profile_identify_the_same_instance() {
    let home = directories::BaseDirs::new().unwrap();
    let explicit = Paths::new(Some(home.home_dir().join(".fwm"))).unwrap();
    let default = Paths::new(None).unwrap();
    assert_eq!(default.config_dir, explicit.config_dir);
    assert_eq!(default.config_file, explicit.config_dir.join("config.toml"));
    assert_eq!(default.state_dir, explicit.config_dir.join("state"));
    assert_eq!(default.ipc_path, explicit.ipc_path);
    assert_eq!(default.pipe_name(), explicit.pipe_name());
}

#[test]
fn isolated_paths_and_pipe_names_are_stable_per_configuration_directory() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("config with 空格");
    let paths = Paths::new(Some(root.clone())).unwrap();
    let canonical = fs::canonicalize(directory.path())
        .unwrap()
        .join("config with 空格");
    assert_eq!(paths.config_dir, canonical);
    assert_eq!(paths.config_file, canonical.join("config.toml"));
    assert_eq!(paths.state_dir, canonical.join("state"));
    for path in [&paths.ipc_path, &paths.lock_file, &paths.log_file] {
        assert!(path.starts_with(&paths.state_dir));
    }
    assert_eq!(
        paths.pipe_name(),
        Paths::new(Some(root)).unwrap().pipe_name()
    );
    assert_ne!(
        paths.pipe_name(),
        Paths::new(Some(directory.path().join("other")))
            .unwrap()
            .pipe_name()
    );
    assert!(paths.pipe_name().starts_with(r"\\.\pipe\fwm-"));
    assert!(
        !paths.config_dir.exists(),
        "resolving paths must not create runtime state"
    );
}

#[test]
fn relative_config_directory_becomes_absolute_without_changing_current_directory() {
    let current = std::env::current_dir().unwrap();
    let relative = PathBuf::from("fwm-test-relative/config");
    let paths = Paths::new(Some(relative.clone())).unwrap();
    assert_eq!(
        paths.config_dir,
        fs::canonicalize(&current)
            .unwrap()
            .join("fwm-test-relative")
            .join("config")
    );
    assert!(paths.config_dir.is_absolute());
    assert_eq!(std::env::current_dir().unwrap(), current);
}

#[test]
fn directory_creation_is_idempotent_and_rejects_regular_files() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(directory.path().join("nested/config"))).unwrap();
    paths.ensure_dirs().unwrap();
    paths.ensure_dirs().unwrap();
    assert!(paths.config_dir.is_dir());
    assert!(paths.state_dir.is_dir());
    let path = directory.path().join("file");
    fs::write(&path, "keep me").unwrap();
    assert!(
        Paths::new(Some(path.clone()))
            .unwrap()
            .ensure_dirs()
            .is_err()
    );
    assert_eq!(fs::read_to_string(path).unwrap(), "keep me");
}

#[cfg(unix)]
#[test]
fn private_directory_permissions_are_restored_on_existing_directories() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(directory.path().join("config"))).unwrap();
    fs::create_dir_all(&paths.state_dir).unwrap();
    for path in [&paths.config_dir, &paths.state_dir] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o777)).unwrap();
    }
    paths.ensure_dirs().unwrap();
    for path in [&paths.config_dir, &paths.state_dir] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}

#[cfg(unix)]
#[test]
fn config_and_runtime_symlinks_are_rejected_without_modifying_the_target() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("external");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    let linked_config = directory.path().join("linked-config");
    symlink(&target, &linked_config).unwrap();
    assert!(
        Paths::new(Some(linked_config))
            .unwrap_err()
            .to_string()
            .contains("symlink")
    );
    let paths = Paths::new(Some(directory.path().join("real-config"))).unwrap();
    fs::create_dir(&paths.config_dir).unwrap();
    symlink(&target, &paths.state_dir).unwrap();
    assert!(
        paths
            .ensure_dirs()
            .unwrap_err()
            .to_string()
            .contains("symlink")
    );
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(fs::read_dir(target).unwrap().count(), 0);
}

#[test]
fn missing_directories_and_dot_spellings_have_one_identity_and_safe_legacy_pipes() {
    let directory = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(directory.path()).unwrap();
    let direct = Paths::new(Some(base.join("profile"))).unwrap();
    // A verbatim Windows PathBuf eagerly normalizes dot components on join.
    // Use the original temp path to preserve an actual alternate spelling.
    let raw = directory.path().join("profile").join(".");
    let mut dotted = Paths::new(Some(raw.clone())).unwrap();
    assert_eq!(direct.config_dir, dotted.config_dir);
    assert_eq!(direct.pipe_name(), dotted.pipe_name());
    assert_eq!(dotted.legacy_config_dirs[0].as_os_str(), raw.as_os_str());
    assert_eq!(dotted.cli_config_dir().as_os_str(), raw.as_os_str());
    assert_eq!(dotted.pipe_names().len(), 2);
    assert_eq!(dotted.pipe_names()[0], direct.pipe_name());
    let expected = dotted.pipe_names();
    dotted.legacy_config_dirs.push(base.join("unrelated"));
    assert_eq!(
        dotted.pipe_names(),
        expected,
        "legacy discovery must never dial another profile"
    );
    dotted.legacy_config_dirs = vec![base.join("unrelated")];
    assert_eq!(dotted.cli_config_dir(), dotted.config_dir);
    assert!(!direct.config_dir.exists());
    assert_eq!(
        absolute_normalized(std::path::Path::new("missing/../profile/./"), &base).unwrap(),
        direct.config_dir
    );
}

#[cfg(unix)]
#[test]
fn symlink_ancestors_are_resolved_before_parent_components() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().unwrap();
    let actual = directory.path().join("actual/child");
    fs::create_dir_all(&actual).unwrap();
    symlink(&actual, directory.path().join("alias")).unwrap();
    let resolved =
        absolute_normalized(&directory.path().join("alias/../profile"), directory.path()).unwrap();
    assert_eq!(
        resolved,
        fs::canonicalize(directory.path().join("actual"))
            .unwrap()
            .join("profile")
    );
}
