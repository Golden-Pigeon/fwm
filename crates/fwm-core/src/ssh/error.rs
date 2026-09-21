use thiserror::Error;

#[derive(Debug, Error)]
pub enum SshError {
    #[error("SSH hop {index} ({alias}, {host}:{port}, known_hosts {path}): {error}; {remedy}", path = .known_hosts.display())]
    Hop {
        index: usize,
        alias: String,
        host: String,
        port: u16,
        known_hosts: std::path::PathBuf,
        remedy: String,
        #[source]
        error: Box<SshError>,
    },
    #[error("SSH configuration: {0}")]
    Configuration(String),
    #[error(
        "unknown host key for {host}: {fingerprint}; inspect and explicitly trust this key first"
    )]
    UnknownHostKey { host: String, fingerprint: String },
    #[error(
        "HOST KEY CHANGED for {host}: received {fingerprint}; check the server identity before editing known_hosts"
    )]
    HostKeyChanged { host: String, fingerprint: String },
    #[error("revoked host key for {0}")]
    RevokedHostKey(String),
    #[error("host certificates are not yet supported; configure a trusted plain host key")]
    HostCertificateUnsupported,
    #[error("SSH authentication requires attention: {0}")]
    Authentication(String),
    #[error("SSH connection or authentication timed out after {0} seconds")]
    Timeout(u64),
    #[error(transparent)]
    Protocol(#[from] russh::Error),
    #[error(transparent)]
    Key(#[from] russh::keys::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl SshError {
    /// Permanent configuration/trust/authentication failures should not spin
    /// in a retry loop. Transport failures can be retried by the supervisor.
    pub fn needs_attention(&self) -> bool {
        if let Self::Hop { error, .. } = self {
            return error.needs_attention();
        }
        matches!(
            self,
            Self::Configuration(_)
                | Self::UnknownHostKey { .. }
                | Self::HostKeyChanged { .. }
                | Self::RevokedHostKey(_)
                | Self::HostCertificateUnsupported
                | Self::Authentication(_)
                | Self::Key(_)
        )
    }
}
