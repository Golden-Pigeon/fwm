use super::ServiceError;
use anyhow::{Context, Result};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

pub(super) struct Definition {
    path: PathBuf,
    previous: Option<Vec<u8>>,
    staged: Vec<u8>,
    committed: bool,
}
impl Definition {
    pub(super) fn stage(path: &Path, content: &str) -> Result<Self> {
        Self::stage_with(path, content, write_atomically)
    }
    fn stage_with(
        path: &Path,
        content: &str,
        write: impl FnOnce(&Path, &[u8]) -> Result<()>,
    ) -> Result<Self> {
        fs::create_dir_all(path.parent().context("service definition has no parent")?)?;
        let previous = read_optional(path)?;
        let guard = Self {
            path: path.to_owned(),
            previous,
            staged: content.as_bytes().to_vec(),
            committed: false,
        };
        write(path, content.as_bytes())?;
        Ok(guard)
    }
    pub(super) fn had_previous(&self) -> bool {
        self.previous.is_some()
    }
    pub(super) fn commit(mut self) {
        self.committed = true;
    }
    pub(super) fn rollback(&mut self) -> Result<()> {
        let current = read_optional(&self.path)?;
        if current != Some(self.staged.clone()) {
            if current == self.previous {
                self.committed = true;
                return Ok(());
            }
            return Err(ServiceError::new(
                "service_definition_conflict",
                "service definition changed outside this operation; rollback did not overwrite it",
            )
            .into());
        }
        restore(&self.path, self.previous.as_deref())?;
        self.committed = true;
        Ok(())
    }
}
impl Drop for Definition {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.rollback();
        }
    }
}

pub(super) fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(ServiceError::new(
            "service_definition_conflict",
            format!("refusing symlink service definition {}", path.display()),
        )
        .into());
    }
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn restore(path: &Path, previous: Option<&[u8]>) -> Result<()> {
    if let Some(previous) = previous {
        write_atomically(path, previous)
    } else {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

pub(super) fn write_atomically(path: &Path, content: &[u8]) -> Result<()> {
    write_atomically_with(path, content, |file, content| {
        file.write_all(content)?;
        Ok(())
    })
}

fn write_atomically_with(
    path: &Path,
    content: &[u8],
    writer: impl FnOnce(&mut std::fs::File, &[u8]) -> Result<()>,
) -> Result<()> {
    let parent = path.parent().context("service definition has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".fwm-service-{}.tmp", uuid::Uuid::new_v4()));
    struct Temporary(PathBuf);
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let _cleanup = Temporary(temporary.clone());
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    writer(&mut file, content)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_staging_failure_keeps_old_definition_and_cleans_temporary_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("service");
        fs::write(&path, "old").unwrap();
        assert!(
            Definition::stage_with(
                &path,
                "new definition",
                |path, bytes| write_atomically_with(path, bytes, |file, bytes| {
                    file.write_all(&bytes[..bytes.len() / 2])?;
                    Err(std::io::Error::other(
                        "injected failure after writing half the temporary definition",
                    )
                    .into())
                })
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        assert!(
            Definition::stage_with(&path, "new", |path, bytes| {
                write_atomically(path, bytes)?;
                Err(std::io::Error::other("injected post-rename durability failure").into())
            })
            .is_err()
        );
        assert_eq!(fs::read_to_string(path).unwrap(), "old");
    }
    #[test]
    fn rollback_never_overwrites_another_writer() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("service");
        fs::write(&path, "old").unwrap();
        let mut first = Definition::stage(&path, "first").unwrap();
        Definition::stage(&path, "second").unwrap().commit();
        assert_eq!(
            first
                .rollback()
                .unwrap_err()
                .downcast_ref::<ServiceError>()
                .unwrap()
                .code,
            "service_definition_conflict"
        );
        drop(first);
        assert_eq!(fs::read_to_string(path).unwrap(), "second");
    }
}
