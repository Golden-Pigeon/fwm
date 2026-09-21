//! Audit assertions describe current behavior, including defects; no fix implied.
use super::*;
use super::io::{AtomicFiles, FileOps, Native};
use crate::model::{ConnectionMode, DesiredState, ForwardSpec, RemoteCleanup, ServerProfile, Tunnel};
use std::{io as stdio, path::PathBuf, sync::{Arc, Mutex}};
use tempfile::NamedTempFile;

fn fixture() -> (tempfile::TempDir, Store, Config) {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(directory.path().into())).unwrap();
    let store = Store::new(paths);
    let mut server = ServerProfile::new("dev");
    server.host = Some("127.0.0.1".into());
    server.port = Some(22);
    let config = Config {
        revision: 1,
        servers: vec![server.clone()],
        forwards: vec![ForwardSpec {
            id: "rule".into(), name: "web".into(), group: None,
            server_id: server.id.clone(),
            tunnel: Tunnel::Local { listen: "127.0.0.1:3000".parse().unwrap(), target: "localhost:80".parse().unwrap() },
            desired_state: DesiredState::Running,
            connection_mode: ConnectionMode::Shared, remote_cleanup: RemoteCleanup::Off,
        }],
        ..Default::default()
    };
    store.commit(&config).unwrap();
    (directory, store, config)
}

struct EditDuringCommit {
    snapshot: PathBuf, candidate: PathBuf, edited: Vec<u8>, fired: Mutex<bool>,
}
impl FileOps for EditDuringCommit {
    fn write(&self, file: &mut fs::File, bytes: &[u8]) -> stdio::Result<()> { Native.write(file, bytes) }
    fn sync_file(&self, file: &fs::File) -> stdio::Result<()> { Native.sync_file(file) }
    fn replace(&self, file: NamedTempFile, path: &Path) -> stdio::Result<()> {
        Native.replace(file, path)?;
        let mut fired = self.fired.lock().unwrap();
        if path == self.snapshot && !*fired {
            *fired = true;
            fs::write(&self.candidate, &self.edited)?;
        }
        Ok(())
    }
    fn sync_directory(&self, path: &Path) -> stdio::Result<()> { Native.sync_directory(path) }
}

#[test]
fn deep_later_editor_save_is_lost_during_control_and_reload() {
    for mode in ["control", "reload"] {
        let (_directory, mut store, mut selected) = fixture();
        let mut edited = selected.clone();
        edited.servers[0].port = Some(2222);
        if mode == "reload" {
            fs::write(&store.paths.config_file, toml::to_string(&edited).unwrap()).unwrap();
            selected = store.read_candidate().unwrap();
        } else {
            selected.forwards[0].desired_state = DesiredState::Stopped;
        }
        selected.revision += 1;
        edited.servers[0].port = Some(3333);
        let hook = Arc::new(EditDuringCommit { snapshot: store.snapshot(),
            candidate: store.paths.config_file.clone(), edited: toml::to_string(&edited).unwrap().into_bytes(),
            fired: Mutex::new(false) });
        store.files = AtomicFiles::with_operations(hook.clone());
        let result = if mode == "control" { store.commit_control_for(&selected, &["rule".into()]) }
                     else { store.commit_reload(&selected) };
        assert!(result.unwrap().is_none(), "the overwritten later edit is not reported");
        assert!(*hook.fired.lock().unwrap());
        let candidate = read_config(&store.paths.config_file).unwrap();
        assert_eq!(candidate.servers[0].port, selected.servers[0].port);
        assert_ne!(candidate.servers[0].port, edited.servers[0].port);
        println!("CONFIRMED later-edit-overwritten mode={mode} editor_port=3333 actual_port={:?} warning=None", candidate.servers[0].port);
    }
}

struct DirectoryFault {
    candidate_directory: PathBuf, snapshot: PathBuf,
    fail_cleanup: bool, snapshot_replacements: Mutex<usize>,
}
impl FileOps for DirectoryFault {
    fn write(&self, file: &mut fs::File, bytes: &[u8]) -> stdio::Result<()> { Native.write(file, bytes) }
    fn sync_file(&self, file: &fs::File) -> stdio::Result<()> { Native.sync_file(file) }
    fn replace(&self, file: NamedTempFile, path: &Path) -> stdio::Result<()> {
        if path == self.snapshot {
            let mut count = self.snapshot_replacements.lock().unwrap();
            *count += 1;
            if self.fail_cleanup && *count == 2 { return Err(stdio::Error::other("injected cleanup replacement failure")); }
        }
        Native.replace(file, path)
    }
    fn sync_directory(&self, directory: &Path) -> stdio::Result<()> {
        if directory == self.candidate_directory { return Err(stdio::Error::other("injected candidate directory sync failure")); }
        Native.sync_directory(directory)
    }
}

#[test]
fn deep_uncertain_candidate_durability_still_retires_stop_protection() {
    for fail_cleanup in [false, true] {
        let (_directory, mut store, mut selected) = fixture();
        let original_candidate = fs::read(&store.paths.config_file).unwrap();
        selected.forwards[0].desired_state = DesiredState::Stopped;
        selected.revision += 1;
        store.files = AtomicFiles::with_operations(Arc::new(DirectoryFault {
            candidate_directory: store.paths.config_dir.clone(), snapshot: store.snapshot(),
            fail_cleanup, snapshot_replacements: Mutex::new(0),
        }));
        let warning = store.commit_control_for(&selected, &["rule".into()]).unwrap().unwrap();
        assert!(warning.contains("durability"));
        let has_overlay = fs::read_to_string(store.snapshot()).unwrap().starts_with("# fwm-control-overrides:");
        assert_eq!(has_overlay, fail_cleanup);
        // Model only the permitted loss of an unsynced candidate rename. This is
        // explicit simulation, not an actual crash or a native FS crash claim.
        fs::write(&store.paths.config_file, original_candidate).unwrap();
        let restarted = Store::new(store.paths.clone());
        assert_eq!(restarted.load().unwrap().config.forwards[0].desired_state, DesiredState::Stopped);
        let merged = restarted.read_candidate().unwrap();
        let expected = if fail_cleanup { DesiredState::Stopped } else { DesiredState::Running };
        assert_eq!(merged.forwards[0].desired_state, expected);
        println!("MODEL uncertain-mirror fail_cleanup={fail_cleanup} has_overlay={has_overlay} next_reload_state={expected:?}");
    }
}

struct MirrorFailure { candidate: PathBuf }
impl FileOps for MirrorFailure {
    fn write(&self, file: &mut fs::File, bytes: &[u8]) -> stdio::Result<()> { Native.write(file, bytes) }
    fn sync_file(&self, file: &fs::File) -> stdio::Result<()> { Native.sync_file(file) }
    fn replace(&self, file: NamedTempFile, path: &Path) -> stdio::Result<()> {
        if path == self.candidate { return Err(stdio::Error::other("injected mirror replacement failure")); }
        Native.replace(file, path)
    }
    fn sync_directory(&self, directory: &Path) -> stdio::Result<()> { Native.sync_directory(directory) }
}

#[test]
fn deep_accumulated_control_records_can_make_a_successful_commit_unreadable() {
    let (_directory, mut store, template) = fixture();
    let mut current = template.clone();
    current.forwards.clear();
    current.revision += 1;
    store.commit(&current).unwrap();
    store.files = AtomicFiles::with_operations(Arc::new(MirrorFailure { candidate: store.paths.config_file.clone() }));
    let mut confirmed = false;
    for generation in 0..7 {
        let mut draft = current.clone();
        let mut rule = template.forwards[0].clone();
        rule.id = format!("{generation}-{}", "i".repeat(180_000));
        rule.name = format!("web-{generation}");
        rule.desired_state = DesiredState::Stopped;
        draft.forwards.push(rule.clone());
        draft.revision += 1;
        draft.validate().unwrap();
        let json_size = serde_json::to_vec(&draft).unwrap().len();
        assert!(json_size < 256 * 1024);
        fs::write(&store.paths.config_file, toml::to_string(&draft).unwrap()).unwrap();
        let candidate = store.read_candidate().unwrap();
        let warning = store.commit_reload(&candidate).unwrap().unwrap();
        assert!(warning.contains("configuration committed"));
        let snapshot_size = fs::metadata(store.snapshot()).unwrap().len();
        if snapshot_size > 1024 * 1024 {
            let load_error = format!("{:#}", store.load().unwrap_err());
            assert!(load_error.contains("configuration file exceeds 1 MiB"));
            assert!(store.read_candidate().is_ok(), "the candidate itself remains valid");
            println!("CONFIRMED unreadable-successful-commit generation={generation} config_json_bytes={json_size} snapshot_bytes={snapshot_size} error={load_error}");
            confirmed = true;
            break;
        }
        current = candidate;
        current.forwards.clear();
        current.revision += 1;
        store.commit_control_for(&current, &[rule.id]).unwrap();
        assert!(store.load().is_ok());
    }
    assert!(confirmed);
}

#[test]
fn deep_control_record_growth_also_occurs_with_standard_uuid_ids() {
    let (_directory, mut store, template) = fixture();
    let mut current = template.clone();
    current.forwards.clear();
    current.revision += 1;
    store.commit(&current).unwrap();
    store.files = AtomicFiles::with_operations(Arc::new(MirrorFailure { candidate: store.paths.config_file.clone() }));
    let mut confirmed = false;
    for generation in 0..24 {
        let mut draft = current.clone();
        for index in 0..512 {
            let mut rule = template.forwards[0].clone();
            rule.id = uuid::Uuid::from_u128((1 + generation * 512 + index) as u128).to_string();
            rule.name = format!("batch-{generation:02}-replacement-rule-{index:03}");
            rule.desired_state = DesiredState::Stopped;
            draft.forwards.push(rule);
        }
        draft.revision += 1;
        draft.validate().unwrap();
        let json_size = serde_json::to_vec(&draft).unwrap().len();
        assert!(json_size < 256 * 1024);
        fs::write(&store.paths.config_file, toml::to_string(&draft).unwrap()).unwrap();
        let candidate = store.read_candidate().unwrap();
        assert!(store.commit_reload(&candidate).unwrap().is_some());
        let snapshot_size = fs::metadata(store.snapshot()).unwrap().len();
        if snapshot_size > 1024 * 1024 {
            assert!(format!("{:#}", store.load().unwrap_err()).contains("configuration file exceeds 1 MiB"));
            let recovery_error = format!("{:#}", store.recover_from_candidate(false).unwrap_err());
            assert!(recovery_error.contains("control intent is unreadable"));
            assert!(recovery_error.contains("exceeds 1 MiB"));
            println!("CONFIRMED standard-uuid-record-growth previous_deleted={} active_rules=512 config_json_bytes={json_size} snapshot_bytes={snapshot_size}", generation * 512);
            println!("CONFIRMED standard-uuid-record-growth ordinary_recovery_rejected=true discard_unreadable_intent_required=true");
            confirmed = true;
            break;
        }
        current = candidate;
        let ids = current.forwards.iter().map(|rule| rule.id.clone()).collect::<Vec<_>>();
        current.forwards.clear();
        current.revision += 1;
        store.commit_control_for(&current, &ids).unwrap();
        assert!(store.load().is_ok());
    }
    assert!(confirmed);
}

#[test]
fn deep_multiple_drafts_down_up_down_preserve_final_intent_and_unrelated_edits() {
    let (_directory, mut store, mut current) = fixture();
    for (index, state) in [DesiredState::Stopped, DesiredState::Running, DesiredState::Stopped].into_iter().enumerate() {
        let mut draft = current.clone();
        draft.servers[0].port = Some(2200 + index as u16);
        draft.forwards[0].desired_state = if state == DesiredState::Stopped { DesiredState::Running } else { DesiredState::Stopped };
        fs::write(&store.paths.config_file, toml::to_string(&draft).unwrap()).unwrap();
        current.forwards[0].desired_state = state;
        current.revision += 1;
        store.commit_control_for(&current, &["rule".into()]).unwrap();
        let merged = store.read_candidate().unwrap();
        assert_eq!(merged.forwards[0].desired_state, state);
        assert_eq!(merged.servers[0].port, draft.servers[0].port);
    }
    store.files = AtomicFiles::with_operations(Arc::new(MirrorFailure { candidate: store.paths.config_file.clone() }));
    let mut merged = store.read_candidate().unwrap();
    merged.revision = current.revision + 1;
    assert!(store.commit_reload(&merged).unwrap().is_some());
    let restarted = Store::new(store.paths.clone());
    assert_eq!(restarted.read_candidate().unwrap().forwards[0].desired_state, DesiredState::Stopped);
    restarted.commit_reload(&merged).unwrap();
    assert!(!fs::read_to_string(restarted.snapshot()).unwrap().starts_with("# fwm-control-overrides:"));
    println!("CONTROL multiple-drafts-last-intent-and-unrelated-edits preserved; successful retry retires overlay");
}

#[test]
fn deep_next_revision_after_i64_max_is_successfully_saved_but_unreadable() {
    for action in ["down", "recover"] {
        let (_directory, store, mut current) = fixture();
        current.revision = i64::MAX as u64 - 1;
        store.commit(&current).unwrap();
        current = store.load().unwrap().config;
        current.revision = current.revision.checked_add(1).unwrap();
        store.commit(&current).unwrap();
        current = store.load().unwrap().config;
        assert_eq!(current.revision, i64::MAX as u64);
        if action == "down" {
            current.forwards[0].desired_state = DesiredState::Stopped;
            current.revision = current.revision.checked_add(1).unwrap();
            assert!(store.commit_control_for(&current, &["rule".into()]).unwrap().is_none());
        } else {
            let result = store.recover_from_candidate(false).unwrap();
            assert!(result.warning.is_none());
            assert_eq!(result.config.forwards[0].desired_state, DesiredState::Stopped);
            assert_eq!(result.config.revision, i64::MAX as u64 + 1);
        }
        let error = format!("{:#}", store.load().unwrap_err());
        assert!(error.contains("u64 value was too large"));
        assert!(store.read_candidate().is_err());
        println!("CONFIRMED revision-roundtrip action={action} readable_before={} successful_after={} warning=None load_error=u64-value-too-large", i64::MAX, i64::MAX as u64 + 1);
    }
}

#[test]
fn deep_unsupported_schema_never_replaces_applied() {
    let (_directory, store, _) = fixture();
    for version in [0, 4, u32::MAX] {
        let mut draft = store.load().unwrap().config;
        draft.schema_version = version;
        fs::write(&store.paths.config_file, toml::to_string(&draft).unwrap()).unwrap();
        let before = fs::read(store.snapshot()).unwrap();
        assert!(store.read_candidate().is_err());
        assert!(store.recover_from_candidate(false).is_err());
        assert_eq!(fs::read(store.snapshot()).unwrap(), before);
    }
    println!("CONTROL unsupported-schema reload/recover validation preserves applied bytes");
}
