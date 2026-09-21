//! Explicit recovery never treats a damaged applied snapshot as an empty config.
use super::*;
use crate::model::DesiredState;
use serde::Serialize;
use std::{io::Read, path::PathBuf};

#[derive(Debug, Serialize)]
pub struct RecoveryReply {
    pub config: Config,
    pub backup_directory: PathBuf,
    pub warning: Option<String>,
}

impl Store {
    /// Caller must hold the daemon instance lock. All recovered rules remain
    /// stopped, so lost intent cannot silently start a restored listener.
    pub fn recover_from_candidate(&self, discard_unreadable_intent: bool) -> Result<RecoveryReply> {
        reject_symlink(&self.snapshot())?;
        let candidate = self.candidate_version()?;
        let CandidateVersion::Present(text) = &candidate else {
            anyhow::bail!("cannot recover without config.toml");
        };
        let mut config = parse_text(text, &self.paths.config_file)?;
        crate::ssh::normalize_config_paths(&mut config, &self.paths.config_dir)?;
        let intent =
            if self.snapshot().exists() && fs::metadata(self.snapshot())?.len() > 1024 * 1024 {
                Err(anyhow::anyhow!(
                    "damaged snapshot exceeds 1 MiB; control intent cannot be read safely"
                ))
            } else {
                control::read_overrides(&self.snapshot())
            };
        let mut warnings = Vec::new();
        let mut overrides = match intent {
            Ok(overrides) => overrides,
            Err(error) if discard_unreadable_intent => {
                warnings.push(format!("unreadable control intent was explicitly discarded ({error:#}); all recovered rules remain stopped"));
                control::Overrides::default()
            },
            Err(error) => return Err(error.context("control intent is unreadable; recovery has not changed any files. Inspect the snapshot, then use `config recover --from-candidate --discard-unreadable-intent` only if restoring the candidate as stopped rules is intended")),
        };
        overrides.apply(&mut config);
        for rule in &mut config.forwards {
            rule.desired_state = DesiredState::Stopped;
        }
        config.revision = self.recovery_revision(config.revision)?;
        config.validate().map_err(anyhow::Error::msg)?;
        overrides.protect_all(&config);
        self.paths.ensure_dirs()?;
        let parent = self.paths.state_dir.join("recovery-backups");
        reject_symlink(&parent)?;
        fs::create_dir_all(&parent)?;
        let backup_directory = parent.join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&backup_directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
            fs::set_permissions(&backup_directory, fs::Permissions::from_mode(0o700))?;
        }
        for (source, name) in [
            (&self.paths.config_file, "config.toml"),
            (&self.snapshot(), "applied.toml"),
        ] {
            if source.exists() {
                reject_symlink(source)?;
                let mut input = fs::File::open(source)?;
                let mut output = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(backup_directory.join(name))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    output.set_permissions(fs::Permissions::from_mode(0o600))?;
                }
                std::io::copy(&mut input.by_ref(), &mut output)?;
                output.sync_all()?;
            }
        }
        #[cfg(unix)]
        {
            fs::File::open(&backup_directory)?.sync_all()?;
            fs::File::open(&parent)?.sync_all()?;
            fs::File::open(&self.paths.state_dir)?.sync_all()?;
        }
        if let Some(warning) = self
            .commit_with_overrides(&config, overrides, Some(candidate))
            .with_context(|| {
                format!(
                    "recovery could not commit; original files were backed up in {}",
                    backup_directory.display()
                )
            })?
        {
            warnings.push(warning);
        }
        Ok(RecoveryReply {
            config,
            backup_directory,
            warning: (!warnings.is_empty()).then(|| warnings.join("; ")),
        })
    }
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;
