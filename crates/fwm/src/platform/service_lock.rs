use super::ServiceError;
use anyhow::Result;
use fs2::FileExt;
use fwm_core::paths::Paths;
use std::fs::{self, File, OpenOptions};

pub(super) struct OperationLock {
    _file: File,
}
impl Drop for OperationLock {
    fn drop(&mut self) {
        // A concurrent fork can retain this open-file description until exec.
        // Explicit unlock releases our operation immediately in that case.
        let _ = FileExt::unlock(&self._file);
    }
}
impl OperationLock {
    pub(super) fn acquire(paths: &Paths) -> Result<Self> {
        paths.ensure_dirs()?;
        let path = paths.state_dir.join("service-operation.lock");
        if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(ServiceError::new(
                "service_lock_error",
                "refusing symlink service operation lock",
            )
            .into());
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Self { _file: file }),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => Err(ServiceError::new("service_busy", "another service or daemon lifecycle operation owns this profile; no service change was made, retry after it finishes").into()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn equivalent_paths_share_one_operation_lock_and_other_profiles_remain_independent() {
        let directory = tempfile::tempdir().unwrap();
        let first = Paths::new(Some(directory.path().join("profile"))).unwrap();
        let equivalent = Paths::new(Some(directory.path().join("profile/.").to_owned())).unwrap();
        let lock = OperationLock::acquire(&first).unwrap();
        assert!(OperationLock::acquire(&equivalent).is_err());
        assert!(
            OperationLock::acquire(&Paths::new(Some(directory.path().join("other"))).unwrap())
                .is_ok()
        );
        drop(lock);
        assert!(OperationLock::acquire(&equivalent).is_ok());
    }
}
