use super::{Stream, auth_error};
use anyhow::{Context, Result, bail};
use fwm_core::paths::Paths;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tokio::net::{UnixListener, UnixStream};

pub struct Listener {
    socket: UnixListener,
    path: PathBuf,
}

fn current_uid() -> libc::uid_t {
    // SAFETY: geteuid is side-effect free and has no preconditions.
    unsafe { libc::geteuid() }
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
    let uid = current_uid();
    PathBuf::from(format!("/tmp/fwm-{uid}/{hash:016x}.sock"))
}

fn private_socket_directory(path: &std::path::Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    // The short endpoint lives under a shared temporary directory. Never
    // follow a pre-existing symlink or change another user's directory.
    if !metadata.is_dir() || metadata.uid() != current_uid() {
        bail!(
            "IPC runtime directory is not a directory owned by this user: {}",
            path.display()
        );
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn validate_socket_directory(path: &Path, uid: libc::uid_t) -> Result<()> {
    let metadata = socket_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != uid {
        return Err(auth_error(format!(
            "socket directory is not a directory owned by this user: {}",
            path.display()
        )));
    }
    if metadata.permissions().mode() & 0o022 != 0 {
        return Err(auth_error(format!(
            "socket directory is writable by another user: {}",
            path.display()
        )));
    }
    Ok(())
}

fn socket_metadata(path: &Path) -> Result<std::fs::Metadata> {
    std::fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            error.into()
        } else {
            auth_error(format!(
                "cannot verify socket path {}: {error}",
                path.display()
            ))
        }
    })
}

fn authenticate_peer(stream: &UnixStream, uid: libc::uid_t) -> Result<()> {
    let peer = stream.peer_cred().map_err(|error| {
        if matches!(
            error.kind(),
            std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
        ) {
            // The peer can exit between connect() and getpeereid(), notably
            // during shutdown on macOS. No request was sent to this stream.
            // Keep the transport error so callers recheck the instance lock;
            // wrong credentials and permission failures remain fatal below.
            error.into()
        } else {
            auth_error(format!("cannot determine socket peer credentials: {error}"))
        }
    })?;
    if peer.uid() != uid {
        return Err(auth_error(format!(
            "socket peer UID {} does not match current user UID {uid}",
            peer.uid()
        )));
    }
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
        if metadata.uid() != current_uid() {
            return Err(auth_error("refusing to replace another user's IPC socket"));
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
        loop {
            let (stream, _) = self.socket.accept().await?;
            // Directory permissions are not sufficient on every Unix. Check
            // kernel peer credentials before the daemon reads any request.
            if authenticate_peer(&stream, current_uid()).is_ok() {
                return Ok(Box::pin(stream));
            }
            drop(stream);
            // A rejected peer must not stop the daemon or starve shutdown.
            tokio::task::yield_now().await;
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub async fn connect(paths: &Paths) -> Result<Stream> {
    connect_socket(&socket_path(paths)).await
}

async fn connect_socket(path: &Path) -> Result<Stream> {
    // This path is also used by read-only commands: never create or chmod the
    // runtime directory while deciding whether a daemon is already running.
    let uid = current_uid();
    validate_socket_directory(path.parent().context("IPC path has no parent")?, uid)?;
    let metadata = socket_metadata(path)?;
    if !metadata.file_type().is_socket() || metadata.uid() != uid {
        return Err(auth_error(format!(
            "endpoint is not a socket owned by this user: {}",
            path.display()
        )));
    }
    let stream = UnixStream::connect(path).await?;
    // Authenticate the connected handle, not only its pathname: a filesystem
    // check alone cannot establish who actually accepted the connection.
    authenticate_peer(&stream, uid)?;
    Ok(Box::pin(stream))
}

#[cfg(test)]
#[path = "ipc_unix_auth_tests.rs"]
mod auth_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn disconnected_peer_is_transport_failure_not_identity_failure() {
        let (stream, peer) = UnixStream::pair().unwrap();
        drop(peer);
        if let Err(error) = authenticate_peer(&stream, current_uid()) {
            assert!(!super::super::is_authentication_error(&error));
            assert!(error.downcast_ref::<std::io::Error>().is_some());
        }
    }

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
