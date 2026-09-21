//! A daemon-owned committed snapshot and an editable TOML candidate.
use std::{fs, path::Path, sync::Mutex};

use anyhow::{Context, Result};

use crate::{model::Config, paths::Paths};

mod candidate;
mod control;
#[cfg(test)]
mod failure_tests;
mod identity;
mod io;
mod recovery;
mod revision;
pub use recovery::RecoveryReply;

#[cfg(test)]
mod migration_tests;

pub struct Store {
    paths: Paths,
    files: io::AtomicFiles,
    candidate_read: Mutex<Option<CandidateVersion>>,
}

const MAX_CONFIG_BYTES: usize = 1024 * 1024;

#[derive(Clone, PartialEq, Eq)]
enum CandidateVersion {
    Missing,
    Present(String),
}

#[derive(Debug)]
pub struct LoadedConfig {
    pub config: Config,
    pub warning: Option<String>,
}

impl Store {
    pub fn new(paths: Paths) -> Self {
        Self {
            paths,
            files: io::AtomicFiles::default(),
            candidate_read: Mutex::new(None),
        }
    }

    fn snapshot(&self) -> std::path::PathBuf {
        self.paths.state_dir.join("applied.toml")
    }

    /// Read-only: offline status must never start a daemon or change user files.
    pub fn load(&self) -> Result<LoadedConfig> {
        reject_symlink(&self.snapshot())?;
        if self.snapshot().exists() {
            let config = self.read_file(&self.snapshot())
                .context("cannot read last committed configuration; using the same --config-dir as this command, stop the daemon, repair config.toml, then run `fwm config recover --from-candidate` (backs up the damaged snapshot and preserves readable control intent)")?;
            let warning = match self.read_candidate_inner() {
                Ok(candidate) if candidate == config => None,
                Ok(_) => Some("config.toml differs from the applied configuration; run config validate and config reload to apply it".into()),
                Err(error) => Some(format!("using last committed configuration; candidate is invalid: {error:#}")),
            };
            return Ok(LoadedConfig { config, warning });
        }
        self.require_uninitialized()?;
        reject_symlink(&self.paths.config_file)?;
        let config = if self.paths.config_file.exists() {
            self.read_candidate()?
        } else {
            *self.candidate_read.lock().unwrap() = Some(CandidateVersion::Missing);
            Config::default()
        };
        Ok(LoadedConfig {
            config,
            warning: None,
        })
    }

    pub fn read_candidate(&self) -> Result<Config> {
        let version = self.candidate_version()?;
        let config = self.parse_candidate_version(&version)?;
        *self.candidate_read.lock().unwrap() = Some(version);
        Ok(config)
    }

    fn read_candidate_inner(&self) -> Result<Config> {
        self.parse_candidate_version(&self.candidate_version()?)
    }

    fn parse_candidate_version(&self, version: &CandidateVersion) -> Result<Config> {
        let CandidateVersion::Present(text) = version else {
            anyhow::bail!("cannot read missing config.toml");
        };
        let mut config = parse_text(text, &self.paths.config_file)?;
        crate::ssh::normalize_config_paths(&mut config, &self.paths.config_dir)?;
        control::read_overrides(&self.snapshot())?.apply(&mut config);
        config.validate().map_err(anyhow::Error::msg)?;
        Ok(config)
    }

    fn candidate_version(&self) -> Result<CandidateVersion> {
        reject_symlink(&self.paths.config_file)?;
        match read_text(&self.paths.config_file) {
            Ok(text) => Ok(CandidateVersion::Present(text)),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(CandidateVersion::Missing)
            }
            Err(error) => Err(error),
        }
    }

    pub fn has_pending_edits(&self) -> Result<bool> {
        reject_symlink(&self.snapshot())?;
        if !self.snapshot().exists() {
            return Ok(false);
        }
        let applied = self.read_file(&self.snapshot())?;
        Ok(self
            .raw_candidate()
            .map_or(true, |candidate| candidate != applied))
    }

    fn raw_candidate(&self) -> Result<Config> {
        self.read_file(&self.paths.config_file)
    }

    fn parse_file(&self, path: &Path) -> Result<Config> {
        let mut config = parse_config(path)?;
        crate::ssh::normalize_config_paths(&mut config, &self.paths.config_dir)?;
        Ok(config)
    }

    fn read_file(&self, path: &Path) -> Result<Config> {
        let config = self.parse_file(path)?;
        config.validate().map_err(anyhow::Error::msg)?;
        Ok(config)
    }

    /// Establish the snapshot on first daemon startup without overwriting edits.
    pub fn initialize(&self, config: &Config) -> Result<()> {
        config.validate().map_err(anyhow::Error::msg)?;
        reject_symlink(&self.snapshot())?;
        if !self.snapshot().exists() {
            self.require_uninitialized()?;
            reject_symlink(&self.paths.config_file)?;
        }
        self.paths.ensure_dirs()?;
        if self.snapshot().exists() {
            let original = fs::read_to_string(self.snapshot())?;
            let version = toml::from_str::<toml::Value>(&original)?
                .get("schema_version")
                .and_then(toml::Value::as_integer);
            if let Some(version) = version.filter(|version| {
                *version >= 1 && *version < i64::from(crate::model::SCHEMA_VERSION)
            }) {
                let backup = self
                    .paths
                    .state_dir
                    .join(format!("applied.v{version}.toml"));
                if !backup.exists() {
                    self.files.persist(
                        self.files
                            .prepare(&self.paths.state_dir, original.as_bytes())?,
                        &backup,
                    )?;
                }
                let version = self.candidate_version().ok().filter(|version| {
                    self.parse_candidate_version(version)
                        .is_ok_and(|candidate| candidate == *config)
                });
                if let Some(warning) = self.commit_with_overrides(
                    config,
                    control::read_overrides(&self.snapshot())?,
                    version,
                )? {
                    tracing::warn!(%warning);
                }
            }
        }
        if !self.snapshot().exists() {
            let candidate = match self.candidate_read.lock().unwrap().clone() {
                Some(version) => version,
                None => self.candidate_version()?,
            };
            if let Some(warning) =
                self.commit_with_overrides(config, control::Overrides::default(), Some(candidate))?
            {
                tracing::warn!(%warning);
            }
        } else {
            self.reserve_revision(config.revision)?;
        }
        Ok(())
    }

    /// Snapshot replacement is the commit point; each replacement is staged.
    /// A later candidate-mirror error is reported as a warning, not a rollback
    /// of a change that has already been durably committed.
    pub fn commit(&self, config: &Config) -> Result<Option<String>> {
        if !self.snapshot().exists() {
            self.require_uninitialized()?;
        }
        let candidate = self.candidate_version()?;
        if self.has_pending_edits()? {
            anyhow::bail!(
                "config.toml has unapplied edits; run config validate and config reload before changing rules"
            );
        }
        let mut overrides = control::read_overrides(&self.snapshot())?;
        overrides.rebase(config);
        self.commit_with_overrides(config, overrides, Some(candidate))
    }

    /// Explicit reload is the only operation allowed to replace a candidate
    /// that differs from the last applied snapshot.
    pub fn commit_reload(&self, config: &Config) -> Result<Option<String>> {
        if !self.snapshot().exists() {
            self.require_uninitialized()?;
        }
        let mut overrides = control::read_overrides(&self.snapshot())?;
        overrides.rebase(config);
        let candidate = self.candidate_read.lock().unwrap().take();
        let candidate = match candidate {
            Some(candidate) => candidate,
            None => self.candidate_version()?,
        };
        self.commit_with_overrides(config, overrides, Some(candidate))
    }
}

pub fn read_config(path: &Path) -> Result<Config> {
    let mut config = parse_config(path)?;
    if let Some(base) = path.parent() {
        crate::ssh::normalize_config_paths(&mut config, base)?;
    }
    config.validate().map_err(anyhow::Error::msg)?;
    Ok(config)
}

fn parse_config(path: &Path) -> Result<Config> {
    parse_text(&read_text(path)?, path)
}

fn read_text(path: &Path) -> Result<String> {
    use std::io::Read;
    reject_symlink(path)?;
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        anyhow::bail!(
            "configuration path is not a regular file: {}",
            path.display()
        );
    }
    if metadata.len() > MAX_CONFIG_BYTES as u64 {
        anyhow::bail!("configuration file exceeds 1 MiB");
    }
    let mut text = String::new();
    fs::File::open(path)?
        .take(MAX_CONFIG_BYTES as u64 + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("cannot read {}", path.display()))?;
    if text.len() > MAX_CONFIG_BYTES {
        anyhow::bail!("configuration file exceeds 1 MiB");
    }
    Ok(text)
}

fn parse_text(text: &str, path: &Path) -> Result<Config> {
    let mut document: toml::Value =
        toml::from_str(text).with_context(|| format!("invalid TOML in {}", path.display()))?;
    identity::normalize(&mut document);
    let mut config: Config = document
        .try_into()
        .with_context(|| format!("invalid configuration in {}", path.display()))?;
    config.migrate().map_err(anyhow::Error::msg)?;
    Ok(config)
}

fn reject_symlink(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        anyhow::bail!("refusing symlink configuration file {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bad_candidate_does_not_replace_last_committed_state() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(temp.path().to_owned())).unwrap();
        let store = Store::new(paths.clone());
        let config = Config {
            revision: 42,
            ..Config::default()
        };
        store.commit(&config).unwrap();
        fs::write(&paths.config_file, "invalid [toml").unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.config.revision, 42);
        assert!(loaded.warning.is_some());
        assert!(store.read_candidate().is_err());
    }
    #[test]
    fn uncommitted_valid_edits_require_reload() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(temp.path().to_owned())).unwrap();
        let store = Store::new(paths.clone());
        store.commit(&Config::default()).unwrap();
        let edited = Config {
            revision: 12,
            ..Config::default()
        };
        fs::write(&paths.config_file, toml::to_string(&edited).unwrap()).unwrap();
        assert_eq!(store.load().unwrap().config.revision, 0);
        assert_eq!(store.read_candidate().unwrap().revision, 12);
        assert!(store.commit(&Config::default()).is_err());
        assert_eq!(store.read_candidate().unwrap().revision, 12);
        store.commit_reload(&edited).unwrap();
        assert_eq!(store.load().unwrap().config.revision, 12);
    }
    #[test]
    fn invalid_commit_keeps_previous_revision() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(temp.path().to_owned())).unwrap();
        let store = Store::new(paths);
        store.commit(&Config::default()).unwrap();
        assert!(
            store
                .commit(&Config {
                    schema_version: 999,
                    ..Config::default()
                })
                .is_err()
        );
        assert_eq!(
            store.load().unwrap().config.schema_version,
            crate::model::SCHEMA_VERSION
        );
    }
    #[test]
    fn explicit_reload_cannot_turn_missing_file_into_empty_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(temp.path().to_owned())).unwrap();
        let store = Store::new(paths.clone());
        assert_eq!(store.load().unwrap().config.revision, 0);
        assert!(store.read_candidate().is_err());
        store
            .commit(&Config {
                revision: 3,
                ..Config::default()
            })
            .unwrap();
        fs::remove_file(&paths.config_file).unwrap();
        assert!(store.read_candidate().is_err());
        assert_eq!(store.load().unwrap().config.revision, 3);
    }
}
