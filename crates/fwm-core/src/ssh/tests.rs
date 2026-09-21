//! Protocol-level trust/authentication regressions, independent of a system sshd.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use russh::{
    keys::{
        Algorithm, PrivateKey,
        ssh_key::{LineEnding, PublicKey},
    },
    server,
};
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

use super::{SshError, check_connection, inspect_host_key, trust_host_key};
use crate::model::{RetryPolicy, ServerProfile};

#[path = "reload_tests.rs"]
mod reload_tests;

struct Fixture {
    _directory: tempfile::TempDir,
    profile: ServerProfile,
    auth_count: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Fixture {
    async fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let identity =
            PrivateKey::random(&mut russh::keys::key::safe_rng(), Algorithm::Ed25519).unwrap();
        let host_key =
            PrivateKey::random(&mut russh::keys::key::safe_rng(), Algorithm::Ed25519).unwrap();
        let config = Arc::new(server::Config {
            keys: vec![host_key],
            auth_rejection_time: std::time::Duration::ZERO,
            ..Default::default()
        });
        let auth_count = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let auth_count_clone = auth_count.clone();
        let allowed = identity.public_key().clone();
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let config = config.clone();
                let handler = TestServer {
                    allowed: allowed.clone(),
                    auth_count: auth_count_clone.clone(),
                };
                tokio::spawn(async move {
                    if let Ok(session) = server::run_stream(config, socket, handler).await {
                        let _ = session.await;
                    }
                });
            }
        });
        let identity_file = directory.path().join("identity");
        identity
            .write_openssh_file(&identity_file, LineEnding::LF)
            .unwrap();
        let ssh_config = directory.path().join("ssh_config");
        std::fs::write(
            &ssh_config,
            "Host *\n IdentityAgent none\n GlobalKnownHostsFile none\n",
        )
        .unwrap();
        let mut profile = ServerProfile::new("fixture");
        profile.host = Some("127.0.0.1".into());
        profile.port = Some(port);
        profile.user = Some("developer".into());
        profile.identity_files = vec![identity_file];
        profile.ssh_config = Some(ssh_config);
        profile.known_hosts = Some(directory.path().join("known_hosts"));
        Self {
            _directory: directory,
            profile,
            auth_count,
            task,
        }
    }

    async fn trust(&self) {
        let info = inspect_host_key(&self.profile, &RetryPolicy::default())
            .await
            .unwrap();
        trust_host_key(&info, &info.fingerprint).unwrap();
    }
}

struct TestServer {
    allowed: PublicKey,
    auth_count: Arc<AtomicUsize>,
}

impl server::Handler for TestServer {
    type Error = russh::Error;
    async fn auth_publickey(
        &mut self,
        user: &str,
        key: &PublicKey,
    ) -> Result<server::Auth, Self::Error> {
        self.auth_count.fetch_add(1, Ordering::Relaxed);
        Ok(
            if user == "developer" && key.key_data() == self.allowed.key_data() {
                server::Auth::Accept
            } else {
                server::Auth::Reject {
                    proceed_with_methods: None,
                    partial_success: false,
                }
            },
        )
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: russh::Channel<server::Msg>,
        host: &str,
        port: u32,
        _: &str,
        _: u32,
        reply: server::ChannelOpenHandle,
        _: &mut server::Session,
    ) -> Result<(), Self::Error> {
        let mut socket = TcpStream::connect((host, port as u16)).await?;
        reply.accept().await;
        tokio::spawn(async move {
            let _ = tokio::io::copy_bidirectional(&mut channel.into_stream(), &mut socket).await;
        });
        Ok(())
    }
}

#[tokio::test]
async fn inspection_sends_no_credentials_and_unknown_or_changed_keys_block_authentication() {
    let fixture = Fixture::start().await;
    let policy = RetryPolicy::default();
    let error = check_connection(&fixture.profile, &policy)
        .await
        .unwrap_err();
    assert!(
        matches!(error, SshError::UnknownHostKey { .. }),
        "{error:?}"
    );
    assert!(error.needs_attention());
    assert_eq!(fixture.auth_count.load(Ordering::Relaxed), 0);
    let info = inspect_host_key(&fixture.profile, &policy).await.unwrap();
    assert_eq!(info.status, "unknown");
    assert_eq!(fixture.auth_count.load(Ordering::Relaxed), 0);
    trust_host_key(&info, &info.fingerprint).unwrap();
    check_connection(&fixture.profile, &policy).await.unwrap();
    assert_eq!(fixture.auth_count.load(Ordering::Relaxed), 1);
    let other = PrivateKey::random(&mut russh::keys::key::safe_rng(), Algorithm::Ed25519).unwrap();
    std::fs::write(
        fixture.profile.known_hosts.as_ref().unwrap(),
        format!(
            "[127.0.0.1]:{} {}\n",
            fixture.profile.port.unwrap(),
            other.public_key().to_openssh().unwrap()
        ),
    )
    .unwrap();
    let error = check_connection(&fixture.profile, &policy)
        .await
        .unwrap_err();
    assert!(
        matches!(error, SshError::HostKeyChanged { .. }),
        "{error:?}"
    );
    assert_eq!(fixture.auth_count.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn native_proxy_jump_authenticates_and_verifies_every_hop() {
    let jump = Fixture::start().await;
    let mut target = Fixture::start().await;
    jump.trust().await;
    target.trust().await;
    // One explicit trust database covers both hops, with different identities
    // resolved from the shared SSH config.
    let jump_keys = std::fs::read_to_string(jump.profile.known_hosts.as_ref().unwrap()).unwrap();
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(target.profile.known_hosts.as_ref().unwrap())
        .unwrap()
        .write_all(jump_keys.as_bytes())
        .unwrap();
    std::fs::write(target.profile.ssh_config.as_ref().unwrap(), format!(
        "Host jump\n HostName 127.0.0.1\n Port {}\n User developer\n IdentityFile {}\nHost *\n IdentityAgent none\n GlobalKnownHostsFile none\n",
        jump.profile.port.unwrap(), jump.profile.identity_files[0].display(),
    )).unwrap();
    target.profile.proxy_jump = vec!["jump".into()];
    check_connection(&target.profile, &RetryPolicy::default())
        .await
        .unwrap();
    assert_eq!(jump.auth_count.load(Ordering::Relaxed), 1);
    assert_eq!(target.auth_count.load(Ordering::Relaxed), 1);
    // Removing only the jump trust entry must prevent credentials reaching it.
    let keys = std::fs::read_to_string(target.profile.known_hosts.as_ref().unwrap()).unwrap();
    let target_entry = keys
        .lines()
        .find(|line| line.contains(&format!(":{} ", target.profile.port.unwrap())))
        .unwrap();
    std::fs::write(
        target.profile.known_hosts.as_ref().unwrap(),
        format!("{target_entry}\n"),
    )
    .unwrap();
    let error = check_connection(&target.profile, &RetryPolicy::default())
        .await
        .unwrap_err();
    assert!(matches!(error, SshError::Hop { .. }));
    assert_eq!(jump.auth_count.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn selected_nested_hop_uses_target_database_and_only_trusts_one_key() {
    let first = Fixture::start().await;
    let second = Fixture::start().await;
    let mut target = Fixture::start().await;
    std::fs::write(target.profile.ssh_config.as_ref().unwrap(), format!(
        "Host first\n HostName 127.0.0.1\n Port {}\n User developer\n IdentityFile {}\nHost second\n HostName 127.0.0.1\n Port {}\n User developer\n IdentityFile {}\n ProxyJump first\nHost *\n IdentityAgent none\n GlobalKnownHostsFile none\n",
        first.profile.port.unwrap(), first.profile.identity_files[0].display(), second.profile.port.unwrap(), second.profile.identity_files[0].display()
    )).unwrap();
    target.profile.proxy_jump = vec!["second".into()];
    let policy = RetryPolicy::default();
    let first_info = super::inspect_hop_key(&target.profile, &policy, "first")
        .await
        .unwrap();
    assert_eq!(
        first_info.known_hosts,
        *target.profile.known_hosts.as_ref().unwrap()
    );
    assert!(trust_host_key(&first_info, "wrong-fingerprint").is_err());
    assert!(!target.profile.known_hosts.as_ref().unwrap().exists());
    trust_host_key(&first_info, &first_info.fingerprint).unwrap();
    assert_eq!(first.auth_count.load(Ordering::Relaxed), 0);
    let second_info = super::inspect_hop_key(&target.profile, &policy, "2")
        .await
        .unwrap();
    assert_eq!(second_info.status, "unknown");
    assert_eq!(second_info.port, second.profile.port.unwrap());
    assert_eq!(second.auth_count.load(Ordering::Relaxed), 0);
    trust_host_key(&second_info, &second_info.fingerprint).unwrap();
    let target_info = inspect_host_key(&target.profile, &policy).await.unwrap();
    assert_eq!(target_info.status, "unknown");
    trust_host_key(&target_info, &target_info.fingerprint).unwrap();
    check_connection(&target.profile, &policy).await.unwrap();
    assert!(!first.profile.known_hosts.as_ref().unwrap().exists());
    assert!(!second.profile.known_hosts.as_ref().unwrap().exists());
    let unknown = super::inspect_hop_key(&target.profile, &policy, "missing")
        .await
        .unwrap_err();
    assert!(unknown.to_string().contains("1:first, 2:second"));
}

#[cfg(unix)]
#[tokio::test]
async fn stalled_agent_becomes_needs_attention_instead_of_an_infinite_reconnect_loop() {
    let mut fixture = Fixture::start().await;
    fixture.trust().await;
    let socket_path = fixture._directory.path().join("agent.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
    let agent = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });
    fixture.profile.identity_files.clear();
    std::fs::write(
        fixture.profile.ssh_config.as_ref().unwrap(),
        format!(
            "Host *\n IdentityFile none\n IdentityAgent {}\n GlobalKnownHostsFile none\n",
            socket_path.display()
        ),
    )
    .unwrap();
    let policy = RetryPolicy {
        connect_timeout_secs: 1,
        ..Default::default()
    };
    let error = check_connection(&fixture.profile, &policy)
        .await
        .unwrap_err();
    agent.abort();
    assert!(matches!(error, SshError::Authentication(_)), "{error:?}");
    assert!(error.needs_attention());
}
