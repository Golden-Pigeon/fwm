use thiserror::Error;

#[derive(Debug, Error)]
pub enum CleanupError {
    #[error("remote recovery state could not be saved: {0}")]
    State(String),
    #[error("remote recovery SSH operation failed: {0}")]
    Ssh(#[from] russh::Error),
    #[error("remote recovery {operation} timed out; the remote result remains unconfirmed")]
    Timeout { operation: &'static str },
    #[error("remote recovery {code}: {message}")]
    Remote { code: String, message: String },
    #[error("remote recovery helper protocol error: {0}")]
    Protocol(String),
    #[error("remote recovery helper I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl CleanupError {
    pub fn needs_attention(&self) -> bool {
        match self {
            Self::Ssh(_) | Self::Timeout { .. } | Self::Io(_) => false,
            Self::Remote { code, .. } => !matches!(
                code.as_str(),
                "busy"
                    | "temporarily_unavailable"
                    | "cleanup_pending"
                    | "unmanaged_conflict"
                    | "helper_disconnected"
            ),
            Self::State(_) | Self::Protocol(_) => true,
        }
    }
}
