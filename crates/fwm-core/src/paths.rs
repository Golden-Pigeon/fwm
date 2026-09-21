use std::{
    io,
    path::{Component, Path, PathBuf},
};

#[cfg(windows)]
mod windows;

#[derive(Clone, Debug)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub state_dir: PathBuf,
    pub ipc_path: PathBuf,
    pub lock_file: PathBuf,
    pub log_file: PathBuf,
    /// Spellings used before canonical directory identities were introduced.
    pub legacy_config_dirs: Vec<PathBuf>,
}

impl Paths {
    pub fn new(config_dir: Option<PathBuf>) -> anyhow::Result<Self> {
        let config_dir = match config_dir {
            Some(path) if path.is_absolute() => path,
            Some(path) => std::env::current_dir()?.join(path),
            None => directories::BaseDirs::new()
                .ok_or_else(|| anyhow::anyhow!("cannot determine user home directory"))?
                .home_dir()
                .join(".fwm"),
        };
        if std::fs::symlink_metadata(&config_dir)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            anyhow::bail!(
                "refusing symlink configuration/runtime directory: {}",
                config_dir.display()
            );
        }
        let original = config_dir;
        let config_dir = absolute_normalized(&original, &std::env::current_dir()?)?;
        let legacy_config_dirs = if original.as_os_str() != config_dir.as_os_str() {
            vec![original]
        } else {
            vec![]
        };
        let state_dir = config_dir.join("state");
        Ok(Self {
            config_file: config_dir.join("config.toml"),
            ipc_path: state_dir.join("daemon.sock"),
            lock_file: state_dir.join("daemon.lock"),
            log_file: state_dir.join("daemon.log"),
            config_dir,
            state_dir,
            legacy_config_dirs,
        })
    }
    pub fn ensure_dirs(&self) -> anyhow::Result<()> {
        private_dir(&self.config_dir)?;
        private_dir(&self.state_dir)?;
        Ok(())
    }
    pub fn pipe_name(&self) -> String {
        Self::pipe_for(&self.config_dir)
    }
    /// The canonical endpoint comes first; only original spellings proven to
    /// identify this same directory are eligible for legacy-daemon fallback.
    pub fn pipe_names(&self) -> Vec<String> {
        let mut names = vec![self.pipe_name()];
        for path in &self.legacy_config_dirs {
            if absolute_normalized(path, &self.config_dir).is_ok_and(|path| path == self.config_dir)
            {
                let name = Self::pipe_for(path);
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        names
    }
    /// Keep a validated original spelling in recovery commands so a new client
    /// can still reach a legacy Windows daemon whose pipe used that spelling.
    pub fn cli_config_dir(&self) -> &Path {
        self.legacy_config_dirs
            .iter()
            .find(|path| {
                absolute_normalized(path, &self.config_dir)
                    .is_ok_and(|canonical| canonical == self.config_dir)
            })
            .map_or(&self.config_dir, PathBuf::as_path)
    }
    fn pipe_for(config_dir: &Path) -> String {
        // Stable per configuration directory; ACLs are enforced by the IPC server.
        let mut hash = 0xcbf29ce484222325u64;
        for byte in config_dir.to_string_lossy().as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        format!(r"\\.\pipe\fwm-{hash:016x}")
    }
}

/// Resolve an absolute filesystem identity without requiring the final path to
/// exist. Existing components are canonicalized before processing `..`, so a
/// symlinked ancestor has the same meaning as it does to the filesystem.
pub fn absolute_normalized(path: &Path, base: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    };
    if !absolute.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path base must be absolute",
        ));
    }
    let mut result = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            Component::Prefix(_) | Component::RootDir => result.push(component.as_os_str()),
            Component::Normal(part) => {
                result.push(part);
                match std::fs::canonicalize(&result) {
                    Ok(canonical) => result = canonical,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
        }
    }
    Ok(result)
}

fn private_dir(path: &Path) -> anyhow::Result<()> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        anyhow::bail!(
            "refusing symlink configuration/runtime directory: {}",
            path.display()
        );
    }
    std::fs::create_dir_all(path)?;
    #[cfg(windows)]
    windows::restrict_directory(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
