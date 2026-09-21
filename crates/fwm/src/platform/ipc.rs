//! Private, current-user transport shared by the daemon and CLI.
use anyhow::Result;
use fwm_core::paths::Paths;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncWrite};

pub trait AsyncStream: AsyncRead + AsyncWrite + Send {}
impl<T: AsyncRead + AsyncWrite + Send> AsyncStream for T {}
pub type Stream = Pin<Box<dyn AsyncStream>>;

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
