use crate::{
    model::{RetryPolicy, ServerProfile},
    ssh::{SshError, check_connection, inspect_host_key, trust_host_key},
};
use russh::{
    keys::{
        key::safe_rng,
        ssh_key::{
            Algorithm, Certificate, HashAlg, LineEnding, PrivateKey, PublicKey,
            certificate::{Builder, CertType},
        },
    },
    server,
};
use std::{
    borrow::Cow,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::{net::TcpListener, task::JoinHandle};

pub fn ed25519() -> Arc<PrivateKey> {
    Arc::new(PrivateKey::random(&mut safe_rng(), Algorithm::Ed25519).unwrap())
}

pub fn rsa() -> Arc<PrivateKey> {
    static KEY: OnceLock<Arc<PrivateKey>> = OnceLock::new();
    KEY.get_or_init(|| {
        Arc::new(PrivateKey::random(&mut safe_rng(), Algorithm::Rsa { hash: None }).unwrap())
    })
    .clone()
}

pub fn certificate(key: &PrivateKey, ca: &PrivateKey) -> Certificate {
    let mut builder = Builder::new(
        vec![7; 32],
        key.public_key().key_data().clone(),
        0,
        u64::MAX,
    )
    .unwrap();
    builder.cert_type(CertType::User).unwrap();
    builder.valid_principal("fixture").unwrap();
    builder.key_id("fwm-auth-test").unwrap();
    builder.sign(ca).unwrap()
}

#[derive(Default)]
pub struct Observations {
    pub offered: Vec<PublicKey>,
    pub authenticated: Vec<PublicKey>,
    pub certificates: usize,
}

pub struct Fixture {
    pub directory: tempfile::TempDir,
    pub profile: ServerProfile,
    pub observations: Arc<Mutex<Observations>>,
    task: JoinHandle<()>,
}

impl Fixture {
    pub async fn new(
        allowed: Vec<PublicKey>,
        ca: Option<PublicKey>,
        rsa_hash: Option<Option<HashAlg>>,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut config = server::Config {
            keys: vec![(*ed25519()).clone()],
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            max_auth_attempts: 32,
            ..Default::default()
        };
        if let Some(hash) = rsa_hash {
            config.preferred.key = Cow::Owned(vec![Algorithm::Ed25519, Algorithm::Rsa { hash }]);
        }
        let config = Arc::new(config);
        let observations = Arc::new(Mutex::new(Observations::default()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let observed = observations.clone();
        let task = tokio::spawn(async move {
            let mut sessions = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    socket = listener.accept() => {
                        let Ok((socket, _)) = socket else { break; };
                        let config = config.clone();
                        let handler = Handler {allowed:allowed.clone(), ca:ca.clone(), observations:observed.clone()};
                        sessions.spawn(async move { if let Ok(session) = server::run_stream(config, socket, handler).await { let _ = session.await; } });
                    }
                    _ = sessions.join_next(), if !sessions.is_empty() => {}
                }
            }
        });
        let mut profile = ServerProfile::new("fixture");
        profile.host = Some("127.0.0.1".into());
        profile.port = Some(port);
        profile.user = Some("fixture".into());
        profile.known_hosts = Some(directory.path().join("known_hosts"));
        profile.ssh_config = Some(directory.path().join("ssh_config"));
        let fixture = Self {
            directory,
            profile,
            observations,
            task,
        };
        fixture.configure(None, false);
        let info = inspect_host_key(&fixture.profile, &Self::policy())
            .await
            .unwrap();
        trust_host_key(&info, &info.fingerprint).unwrap();
        fixture
    }

    pub fn configure(&self, agent: Option<&std::path::Path>, identities_only: bool) {
        std::fs::write(self.profile.ssh_config.as_ref().unwrap(), format!(
            "Host *\n IdentityAgent {}\n IdentitiesOnly {}\n IdentityFile none\n GlobalKnownHostsFile none\n",
            agent.map(|path|path.to_string_lossy().into_owned()).unwrap_or_else(||"none".into()),
            if identities_only {"yes"} else {"no"}
        )).unwrap();
    }

    pub fn identity(&mut self, name: &str, key: &PrivateKey) -> PathBuf {
        let path = self.directory.path().join(name);
        key.write_openssh_file(&path, LineEnding::LF).unwrap();
        key.public_key()
            .write_openssh_file(super::super::suffix(&path, ".pub"))
            .unwrap();
        self.profile.identity_files.push(path.clone());
        path
    }

    pub fn policy() -> RetryPolicy {
        RetryPolicy {
            connect_timeout_secs: 4,
            ..Default::default()
        }
    }

    pub async fn check(&self) -> Result<(), SshError> {
        check_connection(&self.profile, &Self::policy()).await
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Handler {
    allowed: Vec<PublicKey>,
    ca: Option<PublicKey>,
    observations: Arc<Mutex<Observations>>,
}

impl server::Handler for Handler {
    type Error = russh::Error;
    async fn auth_publickey_offered(
        &mut self,
        _: &str,
        key: &PublicKey,
    ) -> Result<server::Auth, Self::Error> {
        self.observations.lock().unwrap().offered.push(key.clone());
        Ok(server::Auth::Accept)
    }
    async fn auth_publickey(
        &mut self,
        user: &str,
        key: &PublicKey,
    ) -> Result<server::Auth, Self::Error> {
        self.observations
            .lock()
            .unwrap()
            .authenticated
            .push(key.clone());
        Ok(
            if user == "fixture"
                && self
                    .allowed
                    .iter()
                    .any(|allowed| allowed.key_data() == key.key_data())
            {
                server::Auth::Accept
            } else {
                server::Auth::reject()
            },
        )
    }
    async fn auth_openssh_certificate(
        &mut self,
        user: &str,
        cert: &Certificate,
    ) -> Result<server::Auth, Self::Error> {
        self.observations.lock().unwrap().certificates += 1;
        let trusted = self
            .ca
            .as_ref()
            .is_some_and(|ca| cert.validate([&ca.fingerprint(HashAlg::Sha256)]).is_ok());
        Ok(
            if user == "fixture"
                && trusted
                && cert.cert_type() == CertType::User
                && cert.valid_principals().iter().any(|name| name == user)
            {
                server::Auth::Accept
            } else {
                server::Auth::reject()
            },
        )
    }
}
