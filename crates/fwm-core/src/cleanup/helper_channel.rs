//! Helper replies and SSH process results must share one message receiver.
use super::{MAX_REPLY, OwnedChannel};
use crate::cleanup::CleanupError;
use russh::{Channel, ChannelMsg, client};
use std::time::Duration;

pub(super) struct HelperChannel {
    channel: OwnedChannel,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit: Option<String>,
    eof: bool,
    closed: bool,
    end_deadline: Option<tokio::time::Instant>,
}

impl HelperChannel {
    pub fn new(channel: Channel<client::Msg>, stderr: Vec<u8>) -> Self {
        Self {
            channel: OwnedChannel(Some(channel)),
            stdout: Vec::new(),
            stderr,
            exit: None,
            eof: false,
            closed: false,
            end_deadline: None,
        }
    }

    pub async fn send(&self, bytes: Vec<u8>) -> Result<(), CleanupError> {
        self.channel.channel().data_bytes(bytes).await?;
        Ok(())
    }

    pub async fn eof(&self) -> Result<(), CleanupError> {
        self.channel.channel().eof().await?;
        Ok(())
    }

    pub async fn read_reply(&mut self, operation: &'static str) -> Result<Vec<u8>, CleanupError> {
        loop {
            if let Some(end) = self.stdout.iter().position(|byte| *byte == b'\n') {
                return Ok(self.stdout.drain(..=end).collect());
            }
            if self.closed {
                return Err(self.close_error(operation));
            }
            // EOF ends stdout, not SSH requests. Keep collecting a late exit
            // status and stderr until close; the caller bounds this wait.
            self.receive().await?;
        }
    }

    pub fn timeout_error(&self, operation: &'static str) -> CleanupError {
        if self.exit.is_some() {
            self.close_error(operation)
        } else {
            CleanupError::Timeout { operation }
        }
    }

    pub async fn wait_closed(&mut self, timeout: Duration) -> CleanupError {
        loop {
            if !self.stdout.is_empty() {
                return Self::idle_output();
            }
            if self.closed {
                return self.close_error("idle monitoring");
            }
            if self.eof || self.exit.is_some() {
                // Once stdout ends, bound collection of the remaining process
                // result. A peer that omits close cannot hide a known failure.
                let deadline = *self
                    .end_deadline
                    .get_or_insert_with(|| tokio::time::Instant::now() + timeout);
                let result = tokio::time::timeout_at(deadline, async {
                    while !self.closed {
                        self.receive().await?;
                        if !self.stdout.is_empty() {
                            return Err(Self::idle_output());
                        }
                    }
                    Ok::<_, CleanupError>(())
                })
                .await;
                return match result {
                    Ok(Err(error)) => error,
                    _ => self.close_error("idle monitoring"),
                };
            }
            if let Err(error) = self.receive().await {
                return error;
            }
        }
    }

    async fn receive(&mut self) -> Result<(), CleanupError> {
        match self.channel.channel_mut().wait().await {
            Some(ChannelMsg::Data { data }) => {
                Self::append(&mut self.stdout, &data, "response")?;
            }
            Some(ChannelMsg::ExtendedData { data, .. }) => {
                Self::append(&mut self.stderr, &data, "stderr")?;
            }
            Some(ChannelMsg::ExitStatus { exit_status }) => {
                self.exit
                    .get_or_insert_with(|| format!("exited with status {exit_status}"));
            }
            Some(ChannelMsg::ExitSignal {
                signal_name,
                error_message,
                ..
            }) => {
                self.exit.get_or_insert_with(|| {
                    format!("terminated by signal {signal_name:?}: {error_message}")
                });
            }
            Some(ChannelMsg::Eof) => self.eof = true,
            Some(ChannelMsg::Close) | None => self.closed = true,
            Some(ChannelMsg::Failure) => {
                return Err(CleanupError::Remote {
                    code: "exec_denied".into(),
                    message: "SSH server rejected the native recovery helper".into(),
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn append(output: &mut Vec<u8>, bytes: &[u8], kind: &str) -> Result<(), CleanupError> {
        if output.len().saturating_add(bytes.len()) > MAX_REPLY {
            return Err(CleanupError::Protocol(format!(
                "helper {kind} exceeds 32 KiB"
            )));
        }
        output.extend_from_slice(bytes);
        Ok(())
    }

    fn close_error(&self, operation: &'static str) -> CleanupError {
        let (code, detail) = match self.exit.as_deref() {
            Some(exit) => ("helper_unavailable", exit),
            None => (
                "helper_disconnected",
                "channel closed without an exit result",
            ),
        };
        CleanupError::Remote {
            code: code.into(),
            message: format!(
                "native recovery helper {detail} during {operation}; the remote result remains unconfirmed: {}",
                String::from_utf8_lossy(&self.stderr).trim()
            ),
        }
    }

    fn idle_output() -> CleanupError {
        CleanupError::Protocol("unexpected recovery helper output while idle".into())
    }
}
