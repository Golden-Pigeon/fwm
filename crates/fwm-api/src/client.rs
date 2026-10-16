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

    /// Exchange one request and return the daemon's reply without enforcing
    /// the current API version. The reply must still echo the request ID;
    /// version-strict callers use `call`. Manager replacement uses this to
    /// retire a daemon one protocol generation behind over its own protocol,
    /// where the Shutdown encoding is unchanged.
    pub async fn exchange(mut self, request: &Request) -> std::io::Result<Option<Response>> {
        write_frame(&mut self.transport, request).await?;
        let response: Option<Response> = read_frame(&mut self.transport).await?;
        if let Some(response) = &response
            && response.request_id != request.request_id
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "daemon reply does not match the submitted request ID",
            ));
        }
        Ok(response)
    }

    /// One daemon connection serves one request. Open a fresh transport for
    /// another request; callers own connection deadlines and retry policy.
    pub async fn call(self, request: &Request) -> std::io::Result<Response> {
        let response = self.exchange(request).await?.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "daemon closed without a response",
            )
        })?;
        if response.api_version != API_VERSION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "daemon API version or request ID mismatch",
            ));
        }
        Ok(response)
    }
}
