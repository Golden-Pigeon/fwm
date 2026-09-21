//! Public Store/path operations using ordinary local files only.
use super::*;
use crate::model::DesiredState;

fn document(version: u32) -> String {
    format!(r#"schema_version = {version}
revision = 4
[[servers]]
id = "server"
name = "dev"
host = "127.0.0.1"
[[forwards]]
name = "pack-3000"
server_id = "server"
kind = "local"
listen = "127.0.0.1:3000"
target = "localhost:80"
desired_state = "running"
[[forwards]]
id = "explicit-rule"
name = "pack-3001"
server_id = "server"
kind = "local"
listen = "127.0.0.1:3001"
target = "localhost:81"
desired_state = "running"
"#)
}

#[test]
fn whole_readonly_identity_then_initialize_and_rename() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(directory.path().into())).unwrap();
    let source = document(3);
    fs::write(&paths.config_file, &source).unwrap();
    let store = Store::new(paths.clone());
    let first = store.load().unwrap().config;
    assert_eq!(first, store.load().unwrap().config);
    assert_eq!(fs::read_to_string(&paths.config_file).unwrap(), source);
    assert!(!paths.state_dir.exists());
    assert_eq!(first.forwards[1].id, "explicit-rule");
    assert!(!first.forwards[0].id.is_empty());
    store.initialize(&first).unwrap();
    let mut edited = store.read_candidate().unwrap();
    edited.forwards[0].name = "renamed".into();
    fs::write(&paths.config_file, toml::to_string(&edited).unwrap()).unwrap();
    let candidate = store.read_candidate().unwrap();
    assert_eq!(candidate.forwards[0].id, first.forwards[0].id);
    assert_eq!(store.load().unwrap().config, first);
    println!("CONTROL read-only implicit IDs stable, no state created; initialized explicit ID survives rename");
}

#[test]
fn whole_legacy_schema_initialization_and_recovery_matrix() {
    for version in [1, 2] {
        for applied_exists in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let paths = Paths::new(Some(directory.path().into())).unwrap();
            let source = document(version);
            fs::write(&paths.config_file, &source).unwrap();
            if applied_exists {
                paths.ensure_dirs().unwrap();
                fs::write(paths.state_dir.join("applied.toml"), &source).unwrap();
            }
            let store = Store::new(paths.clone());
            let loaded = store.load().unwrap().config;
            assert_eq!(loaded.schema_version, 3);
            assert_eq!(loaded.revision, 5);
            assert!(loaded.forwards.iter().all(|rule| rule.group.as_deref() == Some("pack")));
            assert_eq!(store.load().unwrap().config, loaded);
            assert_eq!(fs::read_to_string(&paths.config_file).unwrap(), source);
            store.initialize(&loaded).unwrap();
            assert_eq!(store.load().unwrap().config, loaded);
            assert_eq!(store.read_candidate().unwrap(), loaded);
            if applied_exists {
                assert_eq!(fs::read_to_string(paths.state_dir.join(format!("applied.v{version}.toml"))).unwrap(), source);
            }
            let recovered = store.recover_from_candidate(false).unwrap();
            assert_eq!(recovered.config.revision, 6);
            assert!(recovered.config.forwards.iter().all(|rule| rule.desired_state == DesiredState::Stopped));
            assert_eq!(recovered.config.forwards[0].id, loaded.forwards[0].id);
            assert_eq!(recovered.config.forwards[1].id, "explicit-rule");
            assert_eq!(store.load().unwrap().config, recovered.config);
            println!("CONTROL schema={version} prior_applied={applied_exists} read_only_revision=5 recovered_revision=6 stable_ids=true stopped=true");
        }
    }
}

#[test]
fn whole_migration_preserves_pending_candidate_and_recovery_backs_up_originals() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(directory.path().into())).unwrap();
    paths.ensure_dirs().unwrap();
    let source = document(2);
    fs::write(paths.state_dir.join("applied.toml"), &source).unwrap();
    let candidate = source.replace("localhost:81", "localhost:8081");
    fs::write(&paths.config_file, &candidate).unwrap();
    let store = Store::new(paths.clone());
    let loaded = store.load().unwrap().config;
    store.initialize(&loaded).unwrap();
    assert_eq!(fs::read_to_string(&paths.config_file).unwrap(), candidate);
    assert!(store.has_pending_edits().unwrap());
    let damaged = "interrupted legacy repair [";
    fs::write(paths.state_dir.join("applied.toml"), damaged).unwrap();
    let restored = store.recover_from_candidate(false).unwrap();
    assert_eq!(fs::read_to_string(restored.backup_directory.join("applied.toml")).unwrap(), damaged);
    assert_eq!(fs::read_to_string(restored.backup_directory.join("config.toml")).unwrap(), candidate);
    assert!(restored.config.forwards.iter().all(|rule| rule.desired_state == DesiredState::Stopped));
    assert_eq!(store.read_candidate().unwrap(), restored.config);
    println!("CONTROL migration preserves pending edits; explicit recovery backs up exact bytes and stops rules");
}

#[cfg(unix)]
#[test]
fn whole_path_alias_and_final_file_symlink_keep_saved_identity() {
    let directory = tempfile::tempdir().unwrap();
    let real = directory.path().join("real");
    fs::create_dir_all(real.join("profile")).unwrap();
    let alias = directory.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let direct = Paths::new(Some(real.join("profile"))).unwrap();
    let aliased = Paths::new(Some(alias.join("profile"))).unwrap();
    assert_eq!(direct.config_dir, aliased.config_dir);
    assert_eq!(direct.pipe_name(), aliased.pipe_name());
    let first = direct.config_dir.join("first.key");
    let second = direct.config_dir.join("second.key");
    fs::write(&first, "first ordinary file").unwrap();
    fs::write(&second, "second ordinary file").unwrap();
    let link = direct.config_dir.join("current.key");
    std::os::unix::fs::symlink(&first, &link).unwrap();
    let source = document(3).replace("host = \"127.0.0.1\"", "host = \"127.0.0.1\"\nidentity_files = [\"current.key\"]");
    fs::write(&direct.config_file, source).unwrap();
    let store = Store::new(direct.clone());
    let loaded = store.load().unwrap().config;
    assert_eq!(loaded.servers[0].identity_files, vec![link.clone()]);
    store.initialize(&loaded).unwrap();
    let before = fs::read(&direct.config_file).unwrap();
    fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&second, &link).unwrap();
    assert_eq!(store.load().unwrap().config, loaded);
    assert_eq!(fs::read(&direct.config_file).unwrap(), before);
    println!("CONTROL ancestor aliases share profile identity; final-file link target changes do not rewrite stored paths");
}
