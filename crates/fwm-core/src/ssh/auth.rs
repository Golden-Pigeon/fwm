use std::{path::PathBuf, sync::Arc};

use russh::{
    client,
    keys::{
        agent::{
            AgentIdentity,
            client::{AgentClient, AgentStream},
        },
        key::PrivateKeyWithHashAlg,
        ssh_key::{Algorithm, HashAlg, PublicKey},
    },
};

use super::{ResolvedServer, SshError};

type DynamicAgent = AgentClient<Box<dyn AgentStream + Send + Unpin>>;

#[path = "auth_signer.rs"]
mod signer;

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;

/// Authentication is deliberately noninteractive: unlocked private keys or an
/// existing agent. Passwords/passphrases are never stored in manager config.
pub(super) async fn authenticate<H: client::Handler<Error = SshError>>(
    handle: &mut client::Handle<H>,
    server: &ResolvedServer,
) -> Result<(), SshError> {
    let mut explanations = Vec::new();
    let mut permitted_agent_keys = Vec::new();
    let mut attempted = Vec::new();
    for path in &server.identity_files {
        // The public sidecar lets IdentitiesOnly select an encrypted key in
        // an already unlocked agent without decrypting the private file.
        let public_path = suffix(path, ".pub");
        if let Ok(text) = std::fs::read_to_string(public_path)
            && let Ok(key) = PublicKey::from_openssh(&text)
        {
            permitted_agent_keys.push(key);
        }
        match std::fs::metadata(path) {
            Ok(_) => {}
            Err(error) => {
                if server.explicit_identity_files {
                    explanations.push(format!("identity file {} is unavailable: {error}; correct IdentityFile or --identity", path.display()));
                }
                continue;
            }
        }
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(key) = PublicKey::from_openssh(&text) {
                permitted_agent_keys.push(key);
                continue;
            }
            if let Ok(certificate) = russh::keys::ssh_key::Certificate::from_openssh(&text) {
                permitted_agent_keys.push(PublicKey::new(certificate.public_key().clone(), ""));
                continue;
            }
        }
        let key = match russh::keys::load_secret_key(path, None) {
            Ok(key) => Arc::new(key),
            Err(e) => {
                explanations.push(format!(
                    "{}: {e}; if this private key is encrypted, unlock it in ssh-agent",
                    path.display()
                ));
                continue;
            }
        };
        permitted_agent_keys.push(key.public_key().clone());
        let hash = match rsa_hash(handle, key.algorithm()).await {
            Ok(hash) => hash,
            Err(SshError::Authentication(message)) => {
                explanations.push(message);
                continue;
            }
            Err(error) => return Err(error),
        };
        let cert_path = suffix(path, "-cert.pub");
        if cert_path.exists() {
            let cert = russh::keys::load_openssh_certificate(&cert_path)
                .map_err(|e| SshError::Authentication(format!("{}: {e}", cert_path.display())))?;
            let result = handle
                .authenticate_certificate_with(
                    &server.user,
                    cert,
                    hash,
                    &mut signer::LocalSigner(key.clone()),
                )
                .await
                .map_err(|error| {
                    SshError::Authentication(format!("certificate signing failed: {error}"))
                })?;
            if result.success() {
                return Ok(());
            }
            explanations.push(identity_failure(
                &format!("certificate file {}", cert_path.display()),
                result,
            ));
        }
        if attempted
            .iter()
            .any(|previous: &PublicKey| previous.key_data() == key.public_key().key_data())
        {
            continue;
        }
        attempted.push(key.public_key().clone());
        let result = handle
            .authenticate_publickey(&server.user, PrivateKeyWithHashAlg::new(key, hash))
            .await?;
        if result.success() {
            return Ok(());
        }
        explanations.push(identity_failure(
            &format!("identity file {}", path.display()),
            result,
        ));
    }
    match open_agent(server).await {
        Ok(Some(mut agent)) => {
            let identities = match agent.request_identities().await {
                Ok(identities) => {
                    if identities.is_empty() {
                        explanations.push("SSH agent returned no identities".into());
                    }
                    identities
                }
                Err(error) => {
                    explanations.push(format!("cannot list agent identities: {error}"));
                    Vec::new()
                }
            };
            for identity in identities {
                let public_key = identity.public_key().into_owned();
                if server.identities_only
                    && !permitted_agent_keys
                        .iter()
                        .any(|key| key.key_data() == public_key.key_data())
                {
                    continue;
                }
                // A plain key already rejected by the server will not become
                // valid merely by asking the agent to sign it again.
                if matches!(identity, AgentIdentity::PublicKey { .. })
                    && attempted
                        .iter()
                        .any(|key| key.key_data() == public_key.key_data())
                {
                    continue;
                }
                let hash = match rsa_hash(handle, public_key.algorithm()).await {
                    Ok(hash) => hash,
                    Err(SshError::Authentication(message)) => {
                        explanations.push(message);
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                if matches!(identity, AgentIdentity::PublicKey { .. }) {
                    attempted.push(public_key);
                }
                let result = match identity {
                    AgentIdentity::PublicKey { key, .. } => {
                        handle
                            .authenticate_publickey_with(&server.user, key, hash, &mut agent)
                            .await
                    }
                    AgentIdentity::Certificate { certificate, .. } => {
                        handle
                            .authenticate_certificate_with(
                                &server.user,
                                certificate,
                                hash,
                                &mut agent,
                            )
                            .await
                    }
                };
                match result {
                    Ok(result) if result.success() => return Ok(()),
                    Ok(_) => {}
                    // Russh is still waiting for the requested signature after
                    // signer failure. Reusing this session for another key
                    // would hang; let the caller discard it with this reason.
                    Err(e) => {
                        return Err(SshError::Authentication(format!(
                            "agent signing failed: {e}; unlock or authorize the key before retrying"
                        )));
                    }
                }
            }
        }
        Ok(None) => explanations.push("no SSH agent is configured".into()),
        Err(e) => explanations.push(format!("SSH agent unavailable at {:?} (authentication process PID {}): {e}; configure IdentityAgent with the current socket path in SSH config, then reconnect this server; doctor reports commands for this instance. A manually started daemon can also inherit the current shell environment with `fwm daemon restart`", agent_socket(server), std::process::id())),
    }
    if explanations.is_empty() {
        explanations.push("the server rejected all available identities".into());
    }
    Err(SshError::Authentication(format!(
        "{}@{}: {}; check the configured identity files and server authentication policy",
        server.user,
        server.host,
        explanations.join("; ")
    )))
}

fn identity_failure(identity: &str, result: client::AuthResult) -> String {
    match result {
        client::AuthResult::Failure {
            remaining_methods,
            partial_success: true,
        } => format!(
            "server accepted {identity} but requires additional authentication: {remaining_methods:?}"
        ),
        client::AuthResult::Failure {
            remaining_methods, ..
        } => format!(
            "server rejected {identity}; remaining authentication methods: {remaining_methods:?}"
        ),
        client::AuthResult::Success => {
            unreachable!("successful authentication returns immediately")
        }
    }
}

pub fn agent_socket(server: &ResolvedServer) -> Option<String> {
    if server
        .identity_agent
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("none"))
    {
        return None;
    }
    let configured = server
        .identity_agent
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("SSH_AUTH_SOCK").ok());
    #[cfg(windows)]
    {
        Some(configured.unwrap_or_else(|| r"\\.\pipe\openssh-ssh-agent".into()))
    }
    #[cfg(not(windows))]
    {
        configured
    }
}

async fn rsa_hash<H: client::Handler<Error = SshError>>(
    handle: &client::Handle<H>,
    algorithm: Algorithm,
) -> Result<Option<HashAlg>, SshError> {
    if !matches!(algorithm, Algorithm::Rsa { .. }) {
        return Ok(None);
    }
    // Never opt into the obsolete SHA-1 ssh-rsa signature as a fallback.
    match handle.best_supported_rsa_hash().await? {
        Some(Some(hash)) => Ok(Some(hash)),
        Some(None) => Err(SshError::Authentication(
            "server supports only obsolete RSA/SHA-1 authentication signatures".into(),
        )),
        None => Ok(Some(HashAlg::Sha512)),
    }
}

fn suffix(path: &std::path::Path, ending: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(ending);
    PathBuf::from(value)
}

async fn open_agent(server: &ResolvedServer) -> Result<Option<DynamicAgent>, russh::keys::Error> {
    if server
        .identity_agent
        .as_deref()
        .is_some_and(|agent| agent.eq_ignore_ascii_case("none"))
    {
        return Ok(None);
    }
    #[cfg(unix)]
    {
        let path = server
            .identity_agent
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("SSH_AUTH_SOCK").ok());
        if let Some(path) = path {
            Ok(Some(AgentClient::connect_uds(path).await?.dynamic()))
        } else {
            Ok(None)
        }
    }
    #[cfg(windows)]
    {
        let path = server
            .identity_agent
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("SSH_AUTH_SOCK").ok())
            .unwrap_or_else(|| r"\\.\pipe\openssh-ssh-agent".into());
        Ok(Some(AgentClient::connect_named_pipe(path).await?.dynamic()))
    }
}
