use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use data_encoding::BASE64;
use hmac::{Hmac, Mac};
use russh::keys::{
    PublicKeyOrCertificate,
    ssh_key::{HashAlg, PublicKey},
};
use serde::{Deserialize, Serialize};
use sha1::Sha1;

use super::{ResolvedServer, SshError, config::wildcard_match};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostKeyInfo {
    pub host: String,
    pub port: u16,
    pub algorithm: String,
    pub fingerprint: String,
    pub public_key: String,
    pub known_hosts: PathBuf,
    pub global_known_hosts: Vec<PathBuf>,
    /// `trusted`, `unknown`, `changed`, or `revoked`.
    pub status: String,
}

impl HostKeyInfo {
    pub(super) fn from_key(server: &ResolvedServer, key: &PublicKey) -> Result<Self, SshError> {
        let status = check_files(
            server.trust_host(),
            server.port,
            key,
            std::iter::once(server.known_hosts.as_path())
                .chain(server.global_known_hosts.iter().map(PathBuf::as_path)),
        )?;
        Ok(Self {
            host: server.trust_host().into(),
            port: server.port,
            algorithm: key.algorithm().to_string(),
            fingerprint: fingerprint(key),
            public_key: key
                .to_openssh()
                .map_err(|e| SshError::Configuration(e.to_string()))?,
            known_hosts: server.known_hosts.clone(),
            global_known_hosts: server.global_known_hosts.clone(),
            status: status.as_str().into(),
        })
    }
}

pub fn verify_host_key(
    server: &ResolvedServer,
    key: &PublicKeyOrCertificate,
) -> Result<bool, SshError> {
    let key = match key {
        PublicKeyOrCertificate::PublicKey { key, .. } => key,
        _ => return Err(SshError::HostCertificateUnsupported),
    };
    let status = check_files(
        server.trust_host(),
        server.port,
        key,
        std::iter::once(server.known_hosts.as_path())
            .chain(server.global_known_hosts.iter().map(PathBuf::as_path)),
    )?;
    validate_status(status, server.trust_host(), key)?;
    Ok(true)
}

/// Persist only the exact key whose fingerprint the user explicitly supplied.
/// Changed/revoked entries are never silently overwritten, even with a matching
/// fingerprint. The CLI must collect trust info from a fresh handshake.
pub fn trust_host_key(info: &HostKeyInfo, expected_fingerprint: &str) -> Result<(), SshError> {
    let key = PublicKey::from_openssh(&info.public_key)
        .map_err(|e| SshError::Configuration(e.to_string()))?;
    if expected_fingerprint != fingerprint(&key) || expected_fingerprint != info.fingerprint {
        return Err(SshError::Configuration(
            "host fingerprint does not match the explicitly approved fingerprint".into(),
        ));
    }
    if info.host.is_empty()
        || info.host.contains([' ', '\t', '\r', '\n', ',', '\0'])
        || info.port == 0
    {
        return Err(SshError::Configuration("invalid host key identity".into()));
    }
    if let Some(parent) = info.known_hosts.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.read(true).append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&info.known_hosts)?;
    fs2::FileExt::lock_exclusive(&file)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    let mut status = check_contents(&contents, &info.host, info.port, &key)?;
    for global in &info.global_known_hosts {
        status = status.merge(read_status(global, &info.host, info.port, &key)?);
    }
    match status {
        TrustStatus::Trusted => return Ok(()),
        TrustStatus::Changed | TrustStatus::Revoked => validate_status(status, &info.host, &key)?,
        TrustStatus::Unknown => {}
    }
    file.seek(SeekFrom::End(0))?;
    if !contents.is_empty() && !contents.ends_with('\n') {
        file.write_all(b"\n")?;
    }
    // Write canonical key bytes, not the caller's potentially annotated text.
    let canonical = PublicKey::new(key.key_data().clone(), "")
        .to_openssh()
        .map_err(|e| SshError::Configuration(e.to_string()))?;
    writeln!(file, "{} {}", host_token(&info.host, info.port), canonical)?;
    file.sync_all()?;
    Ok(())
}

fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

#[derive(Clone, Copy)]
enum TrustStatus {
    Unknown,
    Trusted,
    Changed,
    Revoked,
}

impl TrustStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Trusted => "trusted",
            Self::Changed => "changed",
            Self::Revoked => "revoked",
        }
    }
    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Revoked, _) | (_, Self::Revoked) => Self::Revoked,
            (Self::Trusted, _) | (_, Self::Trusted) => Self::Trusted,
            (Self::Changed, _) | (_, Self::Changed) => Self::Changed,
            _ => Self::Unknown,
        }
    }
}

fn validate_status(status: TrustStatus, host: &str, key: &PublicKey) -> Result<(), SshError> {
    match status {
        TrustStatus::Trusted => Ok(()),
        TrustStatus::Unknown => Err(SshError::UnknownHostKey {
            host: host.into(),
            fingerprint: fingerprint(key),
        }),
        TrustStatus::Changed => Err(SshError::HostKeyChanged {
            host: host.into(),
            fingerprint: fingerprint(key),
        }),
        TrustStatus::Revoked => Err(SshError::RevokedHostKey(host.into())),
    }
}

fn check_files<'a>(
    host: &str,
    port: u16,
    key: &PublicKey,
    files: impl Iterator<Item = &'a Path>,
) -> Result<TrustStatus, SshError> {
    let mut status = TrustStatus::Unknown;
    for file in files {
        status = status.merge(read_status(file, host, port, key)?);
    }
    Ok(status)
}

fn read_status(
    path: &Path,
    host: &str,
    port: u16,
    key: &PublicKey,
) -> Result<TrustStatus, SshError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(TrustStatus::Unknown),
        Err(e) => {
            return Err(SshError::Configuration(format!(
                "cannot read {}: {e}",
                path.display()
            )));
        }
    };
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    check_contents(&contents, host, port, key)
}

fn check_contents(
    contents: &str,
    host: &str,
    port: u16,
    key: &PublicKey,
) -> Result<TrustStatus, SshError> {
    let token = host_token(host, port);
    let mut status = TrustStatus::Unknown;
    for (index, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let first = parts.next().unwrap_or_default();
        let (marker, hosts) = if first.starts_with('@') {
            (Some(first), parts.next().unwrap_or_default())
        } else {
            (None, first)
        };
        if !host_matches(hosts, &token) {
            continue;
        }
        let algorithm = parts.next().ok_or_else(|| invalid_line(index))?;
        let encoded = parts.next().ok_or_else(|| invalid_line(index))?;
        let stored = PublicKey::from_openssh(&format!("{algorithm} {encoded}"))
            .map_err(|_| invalid_line(index))?;
        let same_key = stored.key_data() == key.key_data();
        match marker {
            Some("@revoked") if same_key => status = TrustStatus::Revoked,
            Some("@revoked" | "@cert-authority") => {}
            Some(marker) => {
                return Err(SshError::Configuration(format!(
                    "unsupported known_hosts marker {marker} at line {}",
                    index + 1
                )));
            }
            None if same_key => status = status.merge(TrustStatus::Trusted),
            None if stored.algorithm() == key.algorithm() => {
                status = status.merge(TrustStatus::Changed)
            }
            None => {}
        }
    }
    Ok(status)
}

fn host_token(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_owned()
    } else {
        format!("[{host}]:{port}")
    }
}

fn invalid_line(index: usize) -> SshError {
    SshError::Configuration(format!(
        "invalid matching known_hosts entry at line {}",
        index + 1
    ))
}

fn host_matches(patterns: &str, host: &str) -> bool {
    let mut matched = false;
    for entry in patterns.split(',') {
        let (negative, entry) = if let Some(rest) = entry.strip_prefix('!') {
            (true, rest)
        } else {
            (false, entry)
        };
        let found = if let Some(hashed) = entry.strip_prefix("|1|") {
            hashed.split_once('|').is_some_and(|(salt, hash)| {
                let (Ok(salt), Ok(hash)) = (
                    BASE64.decode(salt.as_bytes()),
                    BASE64.decode(hash.as_bytes()),
                ) else {
                    return false;
                };
                Hmac::<Sha1>::new_from_slice(&salt).is_ok_and(|mut mac| {
                    mac.update(host.as_bytes());
                    mac.verify_slice(&hash).is_ok()
                })
            })
        } else {
            wildcard_match(entry, host)
        };
        if negative && found {
            return false;
        }
        if found {
            matched = true;
        }
    }
    matched
}

#[cfg(test)]
mod tests {
    use super::*;
    const KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILC/a8g/qIyKSQqVKJHLSxIygmrkrBjXBGPicUiAaFRS";

    #[test]
    fn hashed_nonstandard_port_and_revocation() {
        let salt = b"deterministic test salt";
        let mut mac = Hmac::<Sha1>::new_from_slice(salt).unwrap();
        mac.update(b"[example.org]:2222");
        let hashed = format!(
            "|1|{}|{}",
            BASE64.encode(salt),
            BASE64.encode(&mac.finalize().into_bytes())
        );
        let key = PublicKey::from_openssh(KEY).unwrap();
        let contents = format!("{hashed}\t{KEY}\tcomment\n");
        assert_eq!(
            check_contents(&contents, "example.org", 2222, &key)
                .unwrap()
                .as_str(),
            "trusted"
        );
        assert_eq!(
            check_contents(&contents, "example.org", 22, &key)
                .unwrap()
                .as_str(),
            "unknown"
        );
        let revoked = format!("{contents}@revoked [example.org]:2222 {KEY}\n");
        assert_eq!(
            check_contents(&revoked, "example.org", 2222, &key)
                .unwrap()
                .as_str(),
            "revoked"
        );
    }

    #[test]
    fn trust_requires_exact_fingerprint_and_does_not_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let key = PublicKey::from_openssh(KEY).unwrap();
        let info = HostKeyInfo {
            host: "example.org".into(),
            port: 2222,
            algorithm: "ssh-ed25519".into(),
            fingerprint: fingerprint(&key),
            public_key: KEY.into(),
            known_hosts: dir.path().join("known_hosts"),
            global_known_hosts: vec![],
            status: "unknown".into(),
        };
        assert!(trust_host_key(&info, "SHA256:wrong").is_err());
        assert!(!info.known_hosts.exists());
        trust_host_key(&info, &info.fingerprint).unwrap();
        trust_host_key(&info, &info.fingerprint).unwrap();
        let contents = std::fs::read_to_string(&info.known_hosts).unwrap();
        assert_eq!(contents.lines().count(), 1);
        assert_eq!(
            check_contents(&contents, "example.org", 2222, &key)
                .unwrap()
                .as_str(),
            "trusted"
        );
    }

    #[test]
    fn changed_key_is_blocked_and_cannot_be_overwritten_by_trust() {
        let dir = tempfile::tempdir().unwrap();
        let replacement = russh::keys::PrivateKey::random(
            &mut russh::keys::key::safe_rng(),
            russh::keys::Algorithm::Ed25519,
        )
        .unwrap();
        let key = replacement.public_key();
        let path = dir.path().join("known_hosts");
        let original = format!("example.org {KEY}\n");
        std::fs::write(&path, &original).unwrap();
        assert_eq!(
            read_status(&path, "example.org", 22, key).unwrap().as_str(),
            "changed"
        );
        let info = HostKeyInfo {
            host: "example.org".into(),
            port: 22,
            algorithm: "ssh-ed25519".into(),
            fingerprint: fingerprint(key),
            public_key: key.to_openssh().unwrap(),
            known_hosts: path.clone(),
            global_known_hosts: vec![],
            status: "changed".into(),
        };
        assert!(matches!(
            trust_host_key(&info, &info.fingerprint),
            Err(SshError::HostKeyChanged { .. })
        ));
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn host_patterns_support_negation_and_public_key_comments_do_not_affect_identity() {
        let mut key = PublicKey::from_openssh(KEY).unwrap();
        key.set_comment("different comment");
        let contents = format!("*.example.org,!prod.example.org {KEY} original comment\n");
        assert_eq!(
            check_contents(&contents, "dev.example.org", 22, &key)
                .unwrap()
                .as_str(),
            "trusted"
        );
        assert_eq!(
            check_contents(&contents, "prod.example.org", 22, &key)
                .unwrap()
                .as_str(),
            "unknown"
        );
    }
}
