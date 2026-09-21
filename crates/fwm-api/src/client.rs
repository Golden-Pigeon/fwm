//! A transport-independent client for future TUI, desktop and web adapters.
use crate::{
    codec::{read_frame, write_frame},
    protocol::{API_VERSION, Request, Response},
};
use tokio::io::{AsyncRead, AsyncWrite};

pub struct Client<T> {
    transport: T,
}

impl<T: AsyncRead + AsyncWrite + Unpin> Client<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// One daemon connection serves one request. Open a fresh transport for
    /// another request; callers own connection deadlines and retry policy.
    pub async fn call(mut self, request: &Request) -> std::io::Result<Response> {
        write_frame(&mut self.transport, request).await?;
        let response: Response = read_frame(&mut self.transport).await?.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "daemon closed without a response",
            )
        })?;
        if response.api_version != API_VERSION || response.request_id != request.request_id {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "daemon API version or request ID mismatch",
            ));
        }
        Ok(response)
    }
}
