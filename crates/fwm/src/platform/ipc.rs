//! Private, current-user transport shared by the daemon and CLI.
use anyhow::Result;
use fwm_core::paths::Paths;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncWrite};

pub trait AsyncStream: AsyncRead + AsyncWrite + Send {}
impl<T: AsyncRead + AsyncWrite + Send> AsyncStream for T {}
pub type Stream = Pin<Box<dyn AsyncStream>>;

#[derive(Debug)]
struct AuthenticationError(String);

impl std::fmt::Display for AuthenticationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "IPC authentication failed: {}", self.0)
    }
}

impl std::error::Error for AuthenticationError {}

pub(super) fn auth_error(message: impl Into<String>) -> anyhow::Error {
    AuthenticationError(message.into()).into()
}

pub fn is_authentication_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<AuthenticationError>().is_some()
}

#[cfg(unix)]
#[path = "ipc_unix.rs"]
mod implementation;
#[cfg(windows)]
#[path = "ipc_windows.rs"]
mod implementation;

pub use implementation::Listener;
#[cfg(windows)]
pub use implementation::current_user_sid;

pub fn bind(paths: &Paths) -> Result<Listener> {
    implementation::bind(paths)
}

pub async fn connect(paths: &Paths) -> Result<Stream> {
    implementation::connect(paths).await
}
