//! Persist control intent together with the applied snapshot, including when a
//! user is in the middle of editing an invalid or unapplied candidate.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::*;
use crate::model::DesiredState;

const PREFIX: &str = "# fwm-control-overrides: ";

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Overrides {
    forwards: BTreeMap<String, Intent>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Intent {
    name: String,
    desired_state: Option<DesiredState>,
}

impl Overrides {
    pub(super) fn apply(&self, config: &mut Config) {
        config.forwards.retain_mut(|forward| {
            // Names are editable and may be reassigned to another live ID.
            // Missing IDs already receive stable identities when parsing.
            let intent = self.forwards.get(&forward.id);
            match intent {
                Some(Intent {
                    desired_state: None,
                    ..
                }) => false,
                Some(Intent {
                    desired_state: Some(state),
                    ..
                }) => {
                    forward.desired_state = *state;
                    true
                }
                None => true,
            }
        });
    }

    pub(super) fn rebase(&mut self, config: &Config) {
        for (id, intent) in &mut self.forwards {
            if let Some(forward) = config.forwards.iter().find(|forward| forward.id == *id) {
                intent.name = forward.name.clone();
                intent.desired_state = Some(forward.desired_state);
            } else {
                intent.desired_state = None;
            }
        }
    }

    pub(super) fn protect_all(&mut self, config: &Config) {
        self.rebase(config);
        for forward in &config.forwards {
            self.forwards.insert(
                forward.id.clone(),
                Intent {
                    name: forward.name.clone(),
                    desired_state: Some(forward.desired_state),
                },
            );
        }
    }
}

pub(super) fn read_overrides(snapshot: &Path) -> Result<Overrides> {
    reject_symlink(snapshot)?;
    if !snapshot.exists() {
        return Ok(Overrides::default());
    }
    let text = read_text(snapshot)?;
    match text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix(PREFIX))
    {
        Some(json) => {
            serde_json::from_str(json).context("invalid pending control intent in applied snapshot")
        }
        None => Ok(Overrides::default()),
    }
}

impl Store {
    /// Apply changed stop/start/delete intent while preserving an editable draft.
    /// Use `commit_control_for` when an explicit command may be a no-op in the
    /// applied config but still needs to override an older draft.
    pub fn commit_control(&self, config: &Config) -> Result<Option<String>> {
        let previous = self.load()?.config;
        let ids = previous
            .forwards
            .iter()
            .filter(|old| {
                config
                    .forwards
                    .iter()
                    .find(|forward| forward.id == old.id)
                    .is_none_or(|forward| forward.desired_state != old.desired_state)
            })
            .map(|forward| forward.id.clone())
            .collect::<Vec<_>>();
        self.commit_control_for(config, &ids)
    }

    /// The selected IDs are explicit so repeated `down` also protects against a
    /// pending draft that would otherwise resurrect an already-stopped rule.
    pub fn commit_control_for(
        &self,
        config: &Config,
        selected: &[String],
    ) -> Result<Option<String>> {
        let candidate = self.candidate_version().ok();
        let previous = self.load()?.config;
        let pending = self.has_pending_edits()?;
        let mut overrides = read_overrides(&self.snapshot())?;
        for id in selected {
            let current = config.forwards.iter().find(|forward| forward.id == *id);
            let original = previous.forwards.iter().find(|forward| forward.id == *id);
            let name = current
                .or(original)
                .with_context(|| format!("unknown controlled forward {id}"))?
                .name
                .clone();
            overrides.forwards.insert(
                id.clone(),
                Intent {
                    name,
                    desired_state: current.map(|forward| forward.desired_state),
                },
            );
        }
        self.commit_with_overrides(config, overrides, candidate.filter(|_| !pending))
    }

    pub(super) fn commit_with_overrides(
        &self,
        config: &Config,
        overrides: Overrides,
        candidate: Option<CandidateVersion>,
    ) -> Result<Option<String>> {
        config.validate().map_err(anyhow::Error::msg)?;
        self.paths.ensure_dirs()?;
        reject_symlink(&self.snapshot())?;
        let plain = toml::to_string_pretty(config)?;
        let has_overrides = !overrides.forwards.is_empty();
        let snapshot_text = if has_overrides {
            format!("{PREFIX}{}\n{plain}", serde_json::to_string(&overrides)?)
        } else {
            plain.clone()
        };
        if snapshot_text.len() > MAX_CONFIG_BYTES {
            anyhow::bail!(
                "configuration was not committed: applied configuration including pending control intent exceeds 1 MiB"
            );
        }
        self.reserve_revision(config.revision)?;
        let mut warnings = vec![];
        if let Some(warning) = self
            .files
            .persist(
                self.files
                    .prepare(&self.paths.state_dir, snapshot_text.as_bytes())?,
                &self.snapshot(),
            )
            .context("configuration was not committed")?
        {
            warnings.push(warning);
        }
        if candidate.is_none() {
            warnings.push("config.toml has pending edits and was preserved; selected start/stop/delete intent is saved and will also be preserved when that draft is reloaded".into());
        } else {
            let mirrored = (|| -> Result<Option<String>> {
                reject_symlink(&self.paths.config_file)?;
                let file = self
                    .files
                    .prepare(&self.paths.config_dir, plain.as_bytes())?;
                self.mirror_candidate(file, candidate.unwrap())
            })();
            if mirrored.is_ok() {
                // initialize() and ordinary daemon mutations share this Store
                // with later reloads. A successful mirror becomes the read
                // version for subsequent operations; do not retain the initial
                // Missing/old candidate token after our own publication.
                *self.candidate_read.lock().unwrap() =
                    Some(CandidateVersion::Present(plain.clone()));
            }
            match mirrored {
                Ok(Some(warning)) => warnings.push(format!("{warning}; pending control intent remains protected")),
                Ok(None) => {
                    if has_overrides {
                        // Clearing is last: a crash before this step leaves an
                        // idempotent overlay, never an unprotected stale draft.
                        match self.files.prepare(&self.paths.state_dir, plain.as_bytes()).and_then(|file| self.files.persist(file, &self.snapshot())) {
                            Ok(Some(warning)) => warnings.push(warning),
                            Ok(None) => {},
                            Err(error) => warnings.push(format!("configuration saved; pending control protection could not be cleared: {error:#}")),
                        }
                    }
                }
                Err(error) => warnings.push(format!("configuration committed, but config.toml could not be updated; pending control intent remains protected: {error:#}")),
            }
        }
        Ok((!warnings.is_empty()).then(|| warnings.join("; ")))
    }
}

#[cfg(test)]
mod tests;
