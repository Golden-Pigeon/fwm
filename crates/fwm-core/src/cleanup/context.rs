//! Durable manager identity and attempt fencing across daemon restarts.
use super::CleanupError;
use crate::{model::ForwardSpec, paths::Paths};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub struct CleanupContext {
    inner: Arc<ContextInner>,
}
struct ContextInner {
    path: PathBuf,
    lock_path: PathBuf,
    serial: Mutex<()>,
    owner_id: String,
}

#[derive(Serialize, Deserialize)]
struct RecoveryState {
    version: u32,
    owner_id: String,
    #[serde(default)]
    generations: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct Claim {
    pub op: &'static str,
    pub owner_id: String,
    pub rule_id: String,
    pub generation: u64,
    pub session_id: String,
    pub listen_host: String,
    pub listen_port: u16,
}

impl CleanupContext {
    pub fn open(paths: &Paths) -> Result<Self, CleanupError> {
        paths
            .ensure_dirs()
            .map_err(|e| CleanupError::State(e.to_string()))?;
        let path = paths.state_dir.join("recovery.json");
        let lock_path = paths.state_dir.join("recovery.lock");
        let lock = lock_file(&lock_path)?;
        let state = if path.exists() {
            load(&path)?
        } else {
            let state = RecoveryState {
                version: 1,
                owner_id: uuid::Uuid::new_v4().to_string(),
                generations: BTreeMap::new(),
            };
            save(&path, &state)?;
            state
        };
        drop(lock);
        Ok(Self {
            inner: Arc::new(ContextInner {
                path,
                lock_path,
                serial: Mutex::new(()),
                owner_id: state.owner_id,
            }),
        })
    }

    pub fn owner_id(&self) -> &str {
        &self.inner.owner_id
    }

    pub(super) fn reserve(&self, spec: &ForwardSpec) -> Result<Claim, CleanupError> {
        let _serial = self
            .inner
            .serial
            .lock()
            .map_err(|_| CleanupError::State("recovery state lock poisoned".into()))?;
        let _lock = lock_file(&self.inner.lock_path)?;
        let mut state = load(&self.inner.path)?;
        if state.owner_id != self.inner.owner_id {
            return Err(CleanupError::State(
                "manager identity changed while running".into(),
            ));
        }
        // Config IDs may be user-written strings. Hash them to a fixed UUID
        // namespace rather than using untrusted input as a remote pathname.
        let digest = Sha1::digest(spec.id.as_bytes());
        let mut key = [0; 16];
        key.copy_from_slice(&digest[..16]);
        let rule_id = uuid::Uuid::from_bytes(key).to_string();
        let generation = state
            .generations
            .get(&rule_id)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| CleanupError::State("recovery generation exhausted".into()))?;
        state.generations.insert(rule_id.clone(), generation);
        save(&self.inner.path, &state)?;
        Ok(Claim {
            op: "claim",
            owner_id: state.owner_id,
            rule_id,
            generation,
            session_id: uuid::Uuid::new_v4().to_string(),
            listen_host: spec.tunnel.listen().ip().to_string(),
            listen_port: spec.tunnel.listen().port(),
        })
    }
}

fn regular(path: &Path) -> Result<(), CleanupError> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(CleanupError::State(format!(
            "refusing symlink {}",
            path.display()
        )));
    }
    Ok(())
}

fn lock_file(path: &Path) -> Result<fs::File, CleanupError> {
    regular(path)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    file.lock_exclusive()?;
    Ok(file)
}

fn load(path: &Path) -> Result<RecoveryState, CleanupError> {
    regular(path)?;
    let bytes = fs::read(path)?;
    let state: RecoveryState =
        serde_json::from_slice(&bytes).map_err(|e| CleanupError::State(e.to_string()))?;
    if state.version != 1 || uuid::Uuid::parse_str(&state.owner_id).is_err() {
        return Err(CleanupError::State(
            "invalid recovery state identity/version; refusing to recreate ownership".into(),
        ));
    }
    Ok(state)
}

fn save(path: &Path, state: &RecoveryState) -> Result<(), CleanupError> {
    regular(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| CleanupError::State("recovery state has no directory".into()))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    let bytes = serde_json::to_vec(state).map_err(|e| CleanupError::State(e.to_string()))?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)
        .map_err(|e| CleanupError::State(e.to_string()))?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ConnectionMode, DesiredState, RemoteCleanup, Tunnel};
    fn rule() -> ForwardSpec {
        ForwardSpec {
            group: None,
            id: "custom-rule-id".into(),
            name: "test".into(),
            server_id: "server".into(),
            tunnel: Tunnel::Remote {
                listen: "127.0.0.1:17890".parse().unwrap(),
                target: "localhost:7890".parse().unwrap(),
            },
            desired_state: DesiredState::Running,
            connection_mode: ConnectionMode::Dedicated,
            remote_cleanup: RemoteCleanup::Verified,
        }
    }
    #[test]
    fn restart_preserves_owner_and_monotonically_advances_rule_generation() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(temp.path().into())).unwrap();
        let first = CleanupContext::open(&paths).unwrap();
        let a = first.reserve(&rule()).unwrap();
        let restarted = CleanupContext::open(&paths).unwrap();
        let b = restarted.reserve(&rule()).unwrap();
        assert_eq!(a.owner_id, b.owner_id);
        assert_eq!(a.rule_id, b.rule_id);
        assert_eq!(b.generation, a.generation + 1);
        assert_ne!(a.session_id, b.session_id);
        let other = tempfile::tempdir().unwrap();
        let independent =
            CleanupContext::open(&Paths::new(Some(other.path().into())).unwrap()).unwrap();
        assert_ne!(first.owner_id(), independent.owner_id());
    }
    #[test]
    fn corrupt_ownership_is_never_silently_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(temp.path().into())).unwrap();
        let context = CleanupContext::open(&paths).unwrap();
        fs::write(paths.state_dir.join("recovery.json"), "invalid").unwrap();
        assert!(context.reserve(&rule()).is_err());
        assert!(CleanupContext::open(&paths).is_err());
    }
}
