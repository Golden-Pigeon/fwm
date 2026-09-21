use super::*;
use std::os::unix::fs::symlink;
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn kernel_credentials_reject_a_different_expected_user_before_data() {
    let (stream, mut peer) = UnixStream::pair().unwrap();
    authenticate_peer(&stream, current_uid()).unwrap();
    let error = authenticate_peer(&stream, current_uid() ^ 1).unwrap_err();
    assert!(super::super::is_authentication_error(&error));
    drop(stream);
    let mut data = [0; 1];
    assert_eq!(peer.read(&mut data).await.unwrap(), 0);
}

#[test]
fn socket_directory_owner_is_checked_without_changing_permissions() {
    let directory = tempfile::tempdir().unwrap();
    let before = std::fs::metadata(directory.path()).unwrap().mode();
    let error = validate_socket_directory(directory.path(), current_uid() ^ 1).unwrap_err();
    assert!(super::super::is_authentication_error(&error));
    assert_eq!(std::fs::metadata(directory.path()).unwrap().mode(), before);
}

#[tokio::test]
async fn writable_runtime_directory_is_rejected_without_contacting_peer_or_chmod() {
    let directory = tempfile::tempdir().unwrap();
    let endpoint = directory.path().join("daemon.sock");
    let listener = UnixListener::bind(&endpoint).unwrap();
    for permissions in [0o770, 0o702, 0o777] {
        std::fs::set_permissions(
            directory.path(),
            std::fs::Permissions::from_mode(permissions),
        )
        .unwrap();
        let error = connect_socket(&endpoint).await.err().unwrap();
        assert!(super::super::is_authentication_error(&error));
        assert_eq!(
            std::fs::metadata(directory.path()).unwrap().mode() & 0o777,
            permissions
        );
    }
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn symlinked_runtime_directory_and_socket_cannot_redirect_client() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("runtime");
    std::fs::create_dir(&runtime).unwrap();
    let endpoint = runtime.join("daemon.sock");
    let listener = UnixListener::bind(&endpoint).unwrap();
    let alias = directory.path().join("alias");
    symlink(&runtime, &alias).unwrap();
    let error = connect_socket(&alias.join("daemon.sock"))
        .await
        .err()
        .unwrap();
    assert!(super::super::is_authentication_error(&error));

    let linked_socket = runtime.join("linked.sock");
    symlink(&endpoint, &linked_socket).unwrap();
    let error = connect_socket(&linked_socket).await.err().unwrap();
    assert!(super::super::is_authentication_error(&error));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn readonly_connect_never_initializes_a_missing_runtime_directory() {
    let directory = tempfile::tempdir().unwrap();
    let endpoint = directory.path().join("absent/daemon.sock");
    let error = connect_socket(&endpoint).await.err().unwrap();
    assert!(!super::super::is_authentication_error(&error));
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::NotFound
    );
    assert!(!endpoint.parent().unwrap().exists());
}

#[tokio::test]
async fn readonly_connect_rejects_non_socket_endpoints_without_replacing_them() {
    let directory = tempfile::tempdir().unwrap();
    let endpoint = directory.path().join("daemon.sock");
    std::fs::write(&endpoint, "preserve this file").unwrap();
    let error = connect_socket(&endpoint).await.err().unwrap();
    assert!(super::super::is_authentication_error(&error));
    assert_eq!(
        std::fs::read_to_string(&endpoint).unwrap(),
        "preserve this file"
    );
}
