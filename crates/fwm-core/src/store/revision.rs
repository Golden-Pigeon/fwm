//! An independent high-water mark survives a missing or damaged snapshot.
use super::*;

impl Store {
    fn revision_path(&self) -> std::path::PathBuf {
        self.paths.state_dir.join("committed-revision")
    }

    fn saved_revision(&self) -> Result<Option<u64>> {
        let path = self.revision_path();
        reject_symlink(&path)?;
        match read_text(&path) {
            Ok(text) => {
                let revision: u64 = text
                    .trim()
                    .parse()
                    .context("invalid committed revision metadata")?;
                anyhow::ensure!(
                    revision <= i64::MAX as u64,
                    "committed revision exceeds TOML integer range"
                );
                Ok(Some(revision))
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn reserve_revision(&self, revision: u64) -> Result<()> {
        let saved = self.saved_revision()?;
        if saved.is_some_and(|saved| saved >= revision) {
            return Ok(());
        }
        let text = revision.to_string();
        let staged = self.files.prepare(&self.paths.state_dir, text.as_bytes())?;
        if let Some(warning) = self.files.persist(staged, &self.revision_path())? {
            anyhow::bail!(
                "configuration was not committed: revision durability could not be confirmed: {warning}"
            );
        }
        Ok(())
    }

    pub(super) fn require_uninitialized(&self) -> Result<()> {
        if self.saved_revision()?.is_some() || self.paths.state_dir.join("recovery.json").exists() {
            anyhow::bail!(
                "last committed configuration is missing from an initialized instance; stop the daemon and run `fwm config recover --from-candidate` to recover with all rules stopped"
            );
        }
        Ok(())
    }

    pub(super) fn recovery_revision(&self, candidate: u64) -> Result<u64> {
        let saved = self.saved_revision()?;
        let snapshot = if self.snapshot().exists() {
            // Reading the revision separately also tolerates damage later in
            // the TOML body of a legacy snapshot without revision metadata.
            let text = match read_text(&self.snapshot()) {
                Ok(text) => text,
                Err(_) if saved.is_some() => String::new(),
                Err(error) => return Err(error),
            };
            let parsed = toml::from_str::<Config>(&text)
                .ok()
                .map(|config| config.revision);
            parsed.or_else(|| {
                text.lines()
                    .take_while(|line| !line.trim_start().starts_with('['))
                    .find_map(|line| {
                        let (key, value) = line.split_once('=')?;
                        (key.trim() == "revision")
                            .then(|| {
                                value
                                    .split('#')
                                    .next()
                                    .unwrap()
                                    .trim()
                                    .replace('_', "")
                                    .parse::<u64>()
                                    .ok()
                            })
                            .flatten()
                    })
            })
        } else {
            None
        };
        if saved.is_none()
            && snapshot.is_none()
            && (self.snapshot().exists() || self.paths.state_dir.join("recovery.json").exists())
        {
            anyhow::bail!(
                "cannot establish the previous revision from the damaged snapshot; restore its revision field or committed-revision metadata before recovery so stale edits cannot be accepted"
            );
        }
        candidate
            .max(saved.unwrap_or(0))
            .max(snapshot.unwrap_or(0))
            .checked_add(1)
            .filter(|revision| *revision <= i64::MAX as u64)
            .context("configuration revision exhausted during recovery")
    }
}
