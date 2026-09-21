//! Publish a candidate without an overwrite rename across the version check.
use super::*;
use tempfile::NamedTempFile;

impl Store {
    pub(super) fn mirror_candidate(
        &self,
        staged: NamedTempFile,
        expected: CandidateVersion,
    ) -> Result<Option<String>> {
        let CandidateVersion::Present(expected) = expected else {
            return self
                .files
                .persist_new(staged, &self.paths.config_file)
                .context(
                    "config.toml was created during this operation; the newer draft was preserved",
                );
        };

        // Taking the current entry and publishing with no-clobber closes the
        // compare/overwrite race with editors that save by atomic rename.
        // The temporary directory stays on the same filesystem.
        let backup = tempfile::Builder::new()
            .prefix(".fwm-candidate-")
            .tempdir_in(&self.paths.config_dir)?;
        let original = backup.path().join("config.toml");
        fs::rename(&self.paths.config_file, &original)
            .context("config.toml changed before it could be mirrored; draft was preserved")?;

        let result = (|| {
            let actual = read_text(&original)?;
            anyhow::ensure!(
                actual == expected,
                "config.toml changed during this operation; the newer draft was preserved"
            );
            let warning = self.files.persist_new(staged, &self.paths.config_file)
                .context("config.toml was saved during mirror publication; the newer draft was preserved")?;
            // Also detect in-place writes through an already-open old handle
            // that complete while publication is in progress.
            anyhow::ensure!(
                read_text(&original)? == expected,
                "the original config.toml changed through an open file handle during publication"
            );
            Ok(warning)
        })();

        match result {
            Ok(warning) => Ok(warning),
            Err(error) => {
                // Hard-link publication is atomic and never replaces a later
                // editor save. Keep both files if another save won the name.
                if fs::hard_link(&original, &self.paths.config_file).is_ok() {
                    #[cfg(unix)]
                    fs::File::open(&self.paths.config_dir)?.sync_all()?;
                    Err(error)
                } else {
                    let directory = backup.keep();
                    Err(error.context(format!(
                        "the current draft was preserved; the displaced draft was retained at {}",
                        directory.join("config.toml").display()
                    )))
                }
            }
        }
    }
}
