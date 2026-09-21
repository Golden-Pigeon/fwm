//! Atomic file replacement with injectable filesystem operations.
use anyhow::Result;
use std::{
    fs,
    io::{self, Write},
    path::Path,
    sync::Arc,
};
use tempfile::NamedTempFile;

pub(super) trait FileOps: Send + Sync {
    fn write(&self, file: &mut fs::File, bytes: &[u8]) -> io::Result<()>;
    fn sync_file(&self, file: &fs::File) -> io::Result<()>;
    fn replace(&self, file: NamedTempFile, path: &Path) -> io::Result<()>;
    fn replace_new(&self, file: NamedTempFile, path: &Path) -> io::Result<()> {
        file.persist_noclobber(path)
            .map(|_| ())
            .map_err(|error| error.error)
    }
    fn sync_directory(&self, directory: &Path) -> io::Result<()>;
}

pub(super) struct Native;
impl FileOps for Native {
    fn write(&self, file: &mut fs::File, bytes: &[u8]) -> io::Result<()> {
        file.write_all(bytes)
    }
    fn sync_file(&self, file: &fs::File) -> io::Result<()> {
        file.sync_all()
    }
    fn replace(&self, file: NamedTempFile, path: &Path) -> io::Result<()> {
        file.persist(path).map(|_| ()).map_err(|e| e.error)
    }
    fn sync_directory(&self, directory: &Path) -> io::Result<()> {
        #[cfg(unix)]
        fs::File::open(directory)?.sync_all()?;
        #[cfg(not(unix))]
        let _ = directory;
        Ok(())
    }
}

pub(super) struct AtomicFiles(Arc<dyn FileOps>);
impl Default for AtomicFiles {
    fn default() -> Self {
        Self(Arc::new(Native))
    }
}
impl AtomicFiles {
    #[cfg(test)]
    pub(super) fn with_operations(ops: Arc<dyn FileOps>) -> Self {
        Self(ops)
    }

    pub(super) fn prepare(&self, directory: &Path, bytes: &[u8]) -> Result<NamedTempFile> {
        let mut file = NamedTempFile::new_in(directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        self.0.write(file.as_file_mut(), bytes)?;
        self.0.sync_file(file.as_file())?;
        Ok(file)
    }

    pub(super) fn persist(&self, file: NamedTempFile, path: &Path) -> Result<Option<String>> {
        self.0.replace(file, path)?;
        self.sync_parent(path)
    }

    pub(super) fn persist_new(&self, file: NamedTempFile, path: &Path) -> Result<Option<String>> {
        self.0.replace_new(file, path)?;
        self.sync_parent(path)
    }

    fn sync_parent(&self, path: &Path) -> Result<Option<String>> {
        if let Some(parent) = path.parent()
            && let Err(error) = self.0.sync_directory(parent)
        {
            return Ok(Some(format!(
                "{} was replaced but directory durability could not be confirmed: {error}",
                path.display()
            )));
        }
        Ok(None)
    }
}
