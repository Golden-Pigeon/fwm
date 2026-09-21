use super::Stream;
use anyhow::{Context, Result, bail};
use fwm_core::paths::Paths;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::PathBuf;
use tokio::net::{UnixListener, UnixStream};

pub struct Listener {
    socket: UnixListener,
    path: PathBuf,
}

fn socket_path(paths: &Paths) -> PathBuf {
    // macOS has the smaller sockaddr_un budget (104 bytes including NUL).
    if paths.ipc_path.as_os_str().as_bytes().len() < 104 {
        return paths.ipc_path.clone();
    }
    let mut hash = 0xcbf29ce484222325u64;
    for byte in paths.config_dir.as_os_str().as_bytes() {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
    }
    // SAFETY: geteuid is side-effect free and has no preconditions.
    let uid = unsafe { libc::geteuid() };
    PathBuf::from(format!("/tmp/fwm-{uid}/{hash:016x}.sock"))
}

fn private_socket_directory(path: &std::path::Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    // The short endpoint lives under a shared temporary directory. Never
    // follow a pre-existing symlink or change another user's directory.
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        bail!(
            "IPC runtime directory is not a directory owned by this user: {}",
            path.display()
        );
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub fn bind(paths: &Paths) -> Result<Listener> {
    let socket_path = socket_path(paths);
    let path = &socket_path;
    let parent = path.parent().context("IPC path has no parent")?;
    private_socket_directory(parent)?;
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if !metadata.file_type().is_socket() {
            bail!("refusing to replace non-socket IPC path {}", path.display());
        }
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            bail!("another daemon is already listening on {}", path.display());
        }
        std::fs::remove_file(path).context("removing stale daemon socket")?;
    }
    let socket = UnixListener::bind(path).context("binding private daemon socket")?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(Listener {
        socket,
        path: path.clone(),
    })
}

impl Listener {
    pub async fn accept(&self) -> Result<Stream> {
        let (stream, _) = self.socket.accept().await?;
        Ok(Box::pin(stream))
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub async fn connect(paths: &Paths) -> Result<Stream> {
    Ok(Box::pin(UnixStream::connect(socket_path(paths)).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn socket_is_private_and_transports_frames() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(directory.path().to_path_buf())).unwrap();
        let listener = bind(&paths).unwrap();
        assert_eq!(
            std::fs::metadata(&paths.ipc_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let mut client = connect(&paths).await.unwrap();
        let mut server = listener.accept().await.unwrap();
        client.write_all(b"hello").await.unwrap();
        let mut bytes = [0; 5];
        server.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"hello");
        assert!(bind(&paths).is_err());
        drop(listener);
        assert!(!paths.ipc_path.exists());
    }

    #[tokio::test]
    async fn never_replaces_a_regular_file() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(directory.path().to_path_buf())).unwrap();
        std::fs::create_dir_all(paths.ipc_path.parent().unwrap()).unwrap();
        std::fs::write(&paths.ipc_path, "important").unwrap();
        assert!(bind(&paths).is_err());
        assert_eq!(
            std::fs::read_to_string(&paths.ipc_path).unwrap(),
            "important"
        );
    }

    #[tokio::test]
    async fn long_configuration_paths_use_a_stable_private_short_endpoint() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("long-profile-".repeat(15));
        let paths = Paths::new(Some(config.clone())).unwrap();
        paths.ensure_dirs().unwrap();
        let endpoint = socket_path(&paths);
        assert!(endpoint.as_os_str().as_bytes().len() < 104);
        assert_eq!(
            endpoint,
            socket_path(&Paths::new(Some(config.join("."))).unwrap())
        );
        let other =
            Paths::new(Some(directory.path().join("different-profile-".repeat(12)))).unwrap();
        assert_ne!(endpoint, socket_path(&other));
        let listener = bind(&paths).unwrap();
        let mut client = connect(&paths).await.unwrap();
        let mut server = listener.accept().await.unwrap();
        client.write_all(b"hello").await.unwrap();
        let mut bytes = [0; 5];
        server.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"hello");
        assert_eq!(
            std::fs::metadata(endpoint.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert!(bind(&paths).is_err());
        drop(listener);
        assert!(!endpoint.exists());
    }
}
