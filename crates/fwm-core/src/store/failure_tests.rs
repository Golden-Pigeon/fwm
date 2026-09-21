use super::io::{AtomicFiles, FileOps, Native};
use super::*;
use crate::model::{
    ConnectionMode, DesiredState, ForwardSpec, RemoteCleanup, ServerProfile, Tunnel,
};
use std::{
    io::{self as stdio, Write},
    sync::{Arc, Mutex},
};
use tempfile::NamedTempFile;

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Write,
    Sync,
    Replace,
    DirectorySync,
}
struct Fault {
    op: Op,
    occurrence: usize,
    seen: Mutex<usize>,
}
impl Fault {
    fn hit(&self, op: Op) -> stdio::Result<()> {
        if self.op == op {
            let mut seen = self.seen.lock().unwrap();
            *seen += 1;
            if *seen == self.occurrence {
                return Err(stdio::Error::other("injected filesystem failure"));
            }
        }
        Ok(())
    }
}
impl FileOps for Fault {
    fn write(&self, file: &mut fs::File, bytes: &[u8]) -> stdio::Result<()> {
        // Exercise partial staging writes, not just failure before any bytes.
        let split = bytes.len() / 2;
        file.write_all(&bytes[..split])?;
        self.hit(Op::Write)?;
        file.write_all(&bytes[split..])
    }
    fn sync_file(&self, file: &fs::File) -> stdio::Result<()> {
        self.hit(Op::Sync)?;
        Native.sync_file(file)
    }
    fn replace(&self, file: NamedTempFile, path: &Path) -> stdio::Result<()> {
        self.hit(Op::Replace)?;
        Native.replace(file, path)
    }
    fn replace_new(&self, file: NamedTempFile, path: &Path) -> stdio::Result<()> {
        self.hit(Op::Replace)?;
        Native.replace_new(file, path)
    }
    fn sync_directory(&self, dir: &Path) -> stdio::Result<()> {
        self.hit(Op::DirectorySync)?;
        Native.sync_directory(dir)
    }
}
fn setup() -> (tempfile::TempDir, Store, Config) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().into())).unwrap();
    let store = Store::new(paths);
    let mut server = ServerProfile::new("dev");
    server.host = Some("127.0.0.1".into());
    let rule = ForwardSpec {
        id: "rule".into(),
        name: "web".into(),
        group: None,
        server_id: server.id.clone(),
        tunnel: Tunnel::Local {
            listen: "127.0.0.1:3000".parse().unwrap(),
            target: "localhost:8080".parse().unwrap(),
        },
        desired_state: DesiredState::Running,
        connection_mode: ConnectionMode::Shared,
        remote_cleanup: RemoteCleanup::Off,
    };
    let config = Config {
        servers: vec![server],
        forwards: vec![rule],
        ..Default::default()
    };
    store.commit(&config).unwrap();
    (dir, store, config)
}
fn fault(store: &mut Store, op: Op, occurrence: usize) {
    store.files = AtomicFiles::with_operations(Arc::new(Fault {
        op,
        // Advancing commits first reserve their revision independently of the
        // snapshot/candidate transaction exercised by these fault positions.
        occurrence: occurrence + 1,
        seen: Mutex::new(0),
    }));
}

#[test]
fn staging_write_sync_and_snapshot_replace_fail_without_changing_either_file() {
    for op in [Op::Write, Op::Sync, Op::Replace] {
        let (_dir, mut store, mut config) = setup();
        let original = fs::read(&store.paths.config_file).unwrap();
        let snapshot = fs::read(store.snapshot()).unwrap();
        config.revision += 1;
        config.forwards[0].desired_state = DesiredState::Stopped;
        fault(&mut store, op, 1);
        assert!(store.commit_control_for(&config, &["rule".into()]).is_err());
        assert_eq!(fs::read(&store.paths.config_file).unwrap(), original);
        assert_eq!(fs::read(store.snapshot()).unwrap(), snapshot);
        assert_eq!(
            fs::read_dir(&store.paths.state_dir).unwrap().count(),
            2,
            "no staged file leaks"
        );
    }
}

struct LaterEditor {
    candidate: std::path::PathBuf,
    text: String,
    edited: Mutex<bool>,
}

impl FileOps for LaterEditor {
    fn write(&self, file: &mut fs::File, bytes: &[u8]) -> stdio::Result<()> {
        Native.write(file, bytes)
    }
    fn sync_file(&self, file: &fs::File) -> stdio::Result<()> {
        Native.sync_file(file)
    }
    fn replace(&self, file: NamedTempFile, path: &Path) -> stdio::Result<()> {
        Native.replace(file, path)?;
        let mut edited = self.edited.lock().unwrap();
        if path.file_name().unwrap() == "applied.toml" && !*edited {
            fs::write(&self.candidate, &self.text)?;
            *edited = true;
        }
        Ok(())
    }
    fn sync_directory(&self, path: &Path) -> stdio::Result<()> {
        Native.sync_directory(path)
    }
}

#[test]
fn editor_save_during_control_or_reload_keeps_new_draft_and_reports_conflict() {
    for reload in [false, true] {
        let (_directory, mut store, mut config) = setup();
        let mut later = config.clone();
        later.servers[0].port = Some(3333);
        let text = toml::to_string_pretty(&later).unwrap();
        if reload {
            config = store.read_candidate().unwrap();
        }
        config.revision += 1;
        config.forwards[0].desired_state = DesiredState::Stopped;
        store.files = AtomicFiles::with_operations(Arc::new(LaterEditor {
            candidate: store.paths.config_file.clone(),
            text: text.clone(),
            edited: Mutex::new(false),
        }));
        let warning = if reload {
            store.commit_reload(&config)
        } else {
            store.commit_control_for(&config, &["rule".into()])
        }
        .unwrap()
        .unwrap();
        assert!(warning.contains("newer draft was preserved"), "{warning}");
        assert_eq!(fs::read_to_string(&store.paths.config_file).unwrap(), text);
        assert_eq!(store.load().unwrap().config, config);
    }
}

#[test]
fn editor_save_between_read_candidate_and_reload_is_not_mirrored_over() {
    let (_directory, store, mut config) = setup();
    let candidate = store.read_candidate().unwrap();
    config.servers[0].port = Some(3333);
    let text = toml::to_string_pretty(&config).unwrap();
    fs::write(&store.paths.config_file, &text).unwrap();
    let warning = store.commit_reload(&candidate).unwrap().unwrap();
    assert!(warning.contains("newer draft was preserved"));
    assert_eq!(fs::read_to_string(&store.paths.config_file).unwrap(), text);
}

#[test]
fn first_initialization_does_not_overwrite_a_candidate_created_after_load() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(Paths::new(Some(directory.path().into())).unwrap());
    let config = store.load().unwrap().config;
    let later = Config {
        revision: 123,
        ..Config::default()
    };
    let text = toml::to_string_pretty(&later).unwrap();
    fs::write(&store.paths.config_file, &text).unwrap();
    store.initialize(&config).unwrap();
    assert_eq!(store.load().unwrap().config, config);
    assert_eq!(fs::read_to_string(&store.paths.config_file).unwrap(), text);
}

#[test]
fn sequential_daemon_commits_refresh_the_version_after_initial_publication() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(Paths::new(Some(directory.path().into())).unwrap());
    let mut config = store.load().unwrap().config;
    store.initialize(&config).unwrap();
    for revision in 1..=3 {
        config.revision = revision;
        assert!(store.commit_reload(&config).unwrap().is_none());
        assert_eq!(read_config(&store.paths.config_file).unwrap(), config);
        assert!(!store.has_pending_edits().unwrap());
    }
}

struct PublicationEditor {
    text: String,
    open_original: Option<fs::File>,
}

impl FileOps for PublicationEditor {
    fn write(&self, file: &mut fs::File, bytes: &[u8]) -> stdio::Result<()> {
        Native.write(file, bytes)
    }
    fn sync_file(&self, file: &fs::File) -> stdio::Result<()> {
        Native.sync_file(file)
    }
    fn replace(&self, file: NamedTempFile, path: &Path) -> stdio::Result<()> {
        Native.replace(file, path)
    }
    fn sync_directory(&self, path: &Path) -> stdio::Result<()> {
        Native.sync_directory(path)
    }
    fn replace_new(&self, file: NamedTempFile, path: &Path) -> stdio::Result<()> {
        if let Some(mut original) = self.open_original.as_ref() {
            Native.replace_new(file, path)?;
            original.set_len(0)?;
            original.write_all(self.text.as_bytes())?;
            original.sync_all()
        } else {
            let mut editor = NamedTempFile::new_in(path.parent().unwrap())?;
            editor.write_all(self.text.as_bytes())?;
            editor.persist(path).map_err(|error| error.error)?;
            Native.replace_new(file, path)
        }
    }
}

#[test]
fn editor_rename_after_version_check_wins_without_losing_either_file() {
    let (_directory, mut store, mut config) = setup();
    let original = fs::read_to_string(&store.paths.config_file).unwrap();
    let mut later = config.clone();
    later.servers[0].port = Some(3333);
    let text = toml::to_string_pretty(&later).unwrap();
    store.files = AtomicFiles::with_operations(Arc::new(PublicationEditor {
        text: text.clone(),
        open_original: None,
    }));
    config.revision += 1;
    config.forwards[0].desired_state = DesiredState::Stopped;
    let warning = store
        .commit_control_for(&config, &["rule".into()])
        .unwrap()
        .unwrap();
    assert!(warning.contains("during mirror publication"), "{warning}");
    assert!(
        warning.contains("displaced draft was retained at"),
        "{warning}"
    );
    assert_eq!(fs::read_to_string(&store.paths.config_file).unwrap(), text);
    let backup = fs::read_dir(&store.paths.config_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".fwm-candidate-")
        })
        .unwrap();
    assert_eq!(
        fs::read_to_string(backup.join("config.toml")).unwrap(),
        original
    );
    assert_eq!(store.load().unwrap().config, config);
    assert_eq!(
        store.read_candidate().unwrap().forwards[0].desired_state,
        DesiredState::Stopped
    );
}

#[test]
fn in_place_save_through_a_moved_handle_is_retained_and_reported() {
    let (_directory, mut store, mut config) = setup();
    let original = fs::OpenOptions::new()
        .write(true)
        .open(&store.paths.config_file)
        .unwrap();
    let mut later = config.clone();
    later.servers[0].port = Some(3333);
    let text = toml::to_string_pretty(&later).unwrap();
    store.files = AtomicFiles::with_operations(Arc::new(PublicationEditor {
        text: text.clone(),
        open_original: Some(original),
    }));
    config.revision += 1;
    config.forwards[0].desired_state = DesiredState::Stopped;
    let warning = store
        .commit_control_for(&config, &["rule".into()])
        .unwrap()
        .unwrap();
    assert!(warning.contains("open file handle"), "{warning}");
    let backup = fs::read_dir(&store.paths.config_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".fwm-candidate-")
        })
        .unwrap();
    assert_eq!(
        fs::read_to_string(backup.join("config.toml")).unwrap(),
        text
    );
    assert_eq!(store.load().unwrap().config, config);
}

#[test]
fn revision_reservation_failure_cannot_replace_the_applied_snapshot() {
    for op in [Op::Write, Op::Sync, Op::Replace, Op::DirectorySync] {
        let (_directory, mut store, mut config) = setup();
        let snapshot = fs::read(store.snapshot()).unwrap();
        let candidate = fs::read(&store.paths.config_file).unwrap();
        store.files = AtomicFiles::with_operations(Arc::new(Fault {
            op,
            occurrence: 1,
            seen: Mutex::new(0),
        }));
        config.revision += 1;
        config.forwards[0].desired_state = DesiredState::Stopped;
        assert!(store.commit_control_for(&config, &["rule".into()]).is_err());
        assert_eq!(fs::read(store.snapshot()).unwrap(), snapshot);
        assert_eq!(fs::read(&store.paths.config_file).unwrap(), candidate);
    }
}

#[test]
fn candidate_directory_sync_failure_keeps_control_protection() {
    let (_directory, mut store, mut config) = setup();
    let original = fs::read(&store.paths.config_file).unwrap();
    config.revision += 1;
    config.forwards[0].desired_state = DesiredState::Stopped;
    fault(&mut store, Op::DirectorySync, 2);
    let warning = store
        .commit_control_for(&config, &["rule".into()])
        .unwrap()
        .unwrap();
    assert!(warning.contains("durability"));
    assert!(
        fs::read_to_string(store.snapshot())
            .unwrap()
            .starts_with("# fwm-control-overrides:")
    );
    // Model a lost, unconfirmed candidate rename. The durable overlay must
    // still prevent a subsequent reload from restoring the running intent.
    fs::write(&store.paths.config_file, original).unwrap();
    let restarted = Store::new(store.paths.clone());
    assert_eq!(
        restarted.read_candidate().unwrap().forwards[0].desired_state,
        DesiredState::Stopped
    );
}

#[test]
fn recovery_mirror_failure_preserves_deleted_ids_and_all_recovered_stops() {
    let (_directory, mut store, config) = setup();
    let mut draft = config.clone();
    let mut survivor = draft.forwards[0].clone();
    survivor.id = "survivor".into();
    survivor.name = "survivor".into();
    survivor.tunnel = Tunnel::Dynamic {
        listen: "127.0.0.1:3001".parse().unwrap(),
    };
    draft.forwards.push(survivor);
    fs::write(
        &store.paths.config_file,
        toml::to_string_pretty(&draft).unwrap(),
    )
    .unwrap();
    let mut removed = config;
    removed.revision += 1;
    removed.forwards.clear();
    store
        .commit_control_for(&removed, &["rule".into()])
        .unwrap();
    let original = fs::read_to_string(store.snapshot()).unwrap();
    let header = original.lines().next().unwrap();
    fs::write(store.snapshot(), format!("{header}\nmalformed = [")).unwrap();
    fault(&mut store, Op::Replace, 2);
    let recovered = store.recover_from_candidate(false).unwrap();
    assert!(
        recovered
            .warning
            .unwrap()
            .contains("configuration committed")
    );
    let restarted = Store::new(store.paths.clone());
    let candidate = restarted.read_candidate().unwrap();
    assert_eq!(candidate.forwards.len(), 1);
    assert_eq!(candidate.forwards[0].id, "survivor");
    assert_eq!(candidate.forwards[0].desired_state, DesiredState::Stopped);
    restarted.commit_reload(&candidate).unwrap();
    assert_eq!(
        restarted.load().unwrap().config.forwards,
        candidate.forwards
    );
}

#[test]
fn mirror_failures_report_committed_state_and_keep_stop_protection_on_restart() {
    for op in [Op::Write, Op::Sync, Op::Replace] {
        let (_dir, mut store, mut config) = setup();
        let original = fs::read(&store.paths.config_file).unwrap();
        config.revision += 1;
        config.forwards[0].desired_state = DesiredState::Stopped;
        fault(&mut store, op, 2);
        let warning = store
            .commit_control_for(&config, &["rule".into()])
            .unwrap()
            .unwrap();
        assert!(warning.contains("configuration committed"));
        assert_eq!(fs::read(&store.paths.config_file).unwrap(), original);
        let restarted = Store::new(store.paths.clone());
        assert_eq!(restarted.load().unwrap().config, config);
        assert_eq!(
            restarted.read_candidate().unwrap().forwards[0].desired_state,
            DesiredState::Stopped
        );
        let candidate = restarted.read_candidate().unwrap();
        restarted.commit_reload(&candidate).unwrap();
        assert_eq!(
            restarted.load().unwrap().config.forwards[0].desired_state,
            DesiredState::Stopped
        );
    }
}

#[test]
fn failed_overlay_cleanup_keeps_a_valid_saved_configuration() {
    for op in [Op::Write, Op::Sync, Op::Replace] {
        let (_dir, mut store, mut config) = setup();
        config.revision += 1;
        config.forwards[0].desired_state = DesiredState::Stopped;
        fault(&mut store, op, 3);
        let warning = store
            .commit_control_for(&config, &["rule".into()])
            .unwrap()
            .unwrap();
        assert!(warning.contains("protection could not be cleared"));
        assert_eq!(read_config(&store.paths.config_file).unwrap(), config);
        assert_eq!(
            Store::new(store.paths.clone()).load().unwrap().config,
            config
        );
        assert!(
            fs::read_to_string(store.snapshot())
                .unwrap()
                .starts_with("# fwm-control-overrides:")
        );
    }
}

#[test]
fn post_replace_directory_sync_failure_is_a_warning_not_a_false_rollback() {
    for occurrence in 1..=3 {
        let (_dir, mut store, mut config) = setup();
        config.revision += 1;
        config.forwards[0].desired_state = DesiredState::Stopped;
        fault(&mut store, Op::DirectorySync, occurrence);
        assert!(
            store
                .commit_control_for(&config, &["rule".into()])
                .unwrap()
                .unwrap()
                .contains("durability")
        );
        assert_eq!(
            Store::new(store.paths.clone()).load().unwrap().config,
            config
        );
        assert_eq!(read_config(&store.paths.config_file).unwrap(), config);
    }
}

#[test]
fn oversized_and_symlink_candidates_are_rejected_without_losing_snapshot() {
    let (dir, store, config) = setup();
    fs::write(&store.paths.config_file, vec![b'#'; 1024 * 1024 + 1]).unwrap();
    assert!(
        store
            .read_candidate()
            .unwrap_err()
            .to_string()
            .contains("1 MiB")
    );
    assert_eq!(store.load().unwrap().config, config);
    #[cfg(unix)]
    {
        let target = dir.path().join("untouched");
        fs::write(&target, "unrelated").unwrap();
        fs::remove_file(&store.paths.config_file).unwrap();
        std::os::unix::fs::symlink(&target, &store.paths.config_file).unwrap();
        assert!(
            store
                .read_candidate()
                .unwrap_err()
                .to_string()
                .contains("symlink")
        );
        assert_eq!(store.load().unwrap().config, config);
        assert_eq!(fs::read_to_string(target).unwrap(), "unrelated");
    }
    #[cfg(not(unix))]
    let _ = dir;
}
