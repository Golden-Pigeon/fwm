use super::{CleanupContext, CleanupError, context::Claim};
use crate::model::ForwardSpec;
use russh::{Channel, ChannelMsg, ChannelStream, client};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const HELPER: &str = include_str!("remote_helper.py");
const MAX_REPLY: usize = 32 * 1024;

/// The helper's session channel stays attached to the dedicated SSH connection.
/// Dropping it deliberately does not remove ownership: an interrupted close may
/// leave sshd holding the port, and the next fenced claim must still find it.
pub struct RemoteLease {
    io: BufReader<ChannelStream<client::Msg>>,
    claim: Claim,
    timeout: Duration,
    reclaimed: bool,
    session_pid: u32,
    released: bool,
}

impl RemoteLease {
    pub async fn claim<H>(
        handle: Arc<client::Handle<H>>,
        context: &CleanupContext,
        spec: &ForwardSpec,
        timeout: Duration,
    ) -> Result<Self, CleanupError>
    where
        H: client::Handler + Send + 'static,
    {
        let claim = context.reserve(spec)?;
        let channel = open_session(handle, timeout).await?;
        let mut owned = OwnedChannel(Some(channel));
        let command = format!("python3 -u -c {}", shell_quote(HELPER));
        let initialize = async {
            owned.channel().exec(true, command).await?;
            let mut stderr = String::new();
            loop {
                match owned.channel_mut().wait().await {
                    Some(ChannelMsg::Success) => return Ok::<(), CleanupError>(()),
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        if stderr.len() < 4096 { stderr.push_str(&String::from_utf8_lossy(&data)); }
                    }
                    Some(ChannelMsg::Failure) => return Err(CleanupError::Remote { code:"exec_denied".into(), message:"SSH server rejected the recovery helper; allow command execution for verified recovery".into() }),
                    Some(ChannelMsg::ExitStatus { exit_status }) => return Err(CleanupError::Remote { code:"helper_unavailable".into(), message:format!("Python 3 recovery helper exited with status {exit_status}: {stderr}") }),
                    Some(ChannelMsg::Close | ChannelMsg::Eof) | None => return Err(CleanupError::Remote { code:"helper_unavailable".into(), message:format!("could not start Python 3 recovery helper: {stderr}") }),
                    _ => {},
                }
            }
        };
        tokio::time::timeout(timeout, initialize)
            .await
            .map_err(|_| CleanupError::Timeout {
                operation: "helper startup",
            })??;
        let stream = owned.take().into_stream();
        let mut lease = Self {
            io: BufReader::new(stream),
            claim,
            timeout,
            reclaimed: false,
            session_pid: 0,
            released: false,
        };
        let request = serde_json::to_value(&lease.claim)
            .map_err(|e| CleanupError::Protocol(e.to_string()))?;
        let response = lease.request(request, "claim").await?;
        if response.get("session_id").and_then(Value::as_str)
            != Some(lease.claim.session_id.as_str())
            || response.get("generation").and_then(Value::as_u64) != Some(lease.claim.generation)
        {
            return Err(CleanupError::Protocol(
                "claim response identity does not match this attempt".into(),
            ));
        }
        lease.reclaimed = response
            .get("reclaimed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        lease.session_pid = response
            .get("session_pid")
            .and_then(Value::as_u64)
            .and_then(|p| u32::try_from(p).ok())
            .ok_or_else(|| CleanupError::Protocol("claim response omitted session PID".into()))?;
        Ok(lease)
    }

    pub fn reclaimed(&self) -> bool {
        self.reclaimed
    }
    pub fn session_pid(&self) -> u32 {
        self.session_pid
    }

    pub async fn confirm(&mut self) -> Result<(), CleanupError> {
        let mut command = self.command("confirm");
        command["forward_ack"] = Value::Bool(true);
        self.request(command, "confirm").await?;
        Ok(())
    }

    /// Supervise the control helper while the listener is established. An
    /// unexpected helper exit must not leave an apparently healthy unmanaged
    /// session waiting until the next unrelated network failure.
    pub async fn wait_closed(&mut self) -> CleanupError {
        match self.io.fill_buf().await {
            Ok([]) => CleanupError::Remote {
                code: "helper_disconnected".into(),
                message: "registered recovery helper exited unexpectedly".into(),
            },
            Ok(_) => CleanupError::Protocol("unexpected recovery helper output while idle".into()),
            Err(error) => CleanupError::Io(error),
        }
    }

    pub async fn release(&mut self) -> Result<(), CleanupError> {
        if self.released {
            return Ok(());
        }
        self.request(self.command("release"), "release").await?;
        self.released = true;
        let _ = self.io.get_mut().shutdown().await;
        Ok(())
    }

    fn command(&self, op: &str) -> Value {
        json!({"op":op,"owner_id":self.claim.owner_id,"rule_id":self.claim.rule_id,
            "generation":self.claim.generation,"session_id":self.claim.session_id})
    }

    async fn request(&mut self, request: Value, op: &'static str) -> Result<Value, CleanupError> {
        let timeout = self.timeout;
        tokio::time::timeout(timeout, async {
            let mut line = serde_json::to_vec(&request).map_err(|e| CleanupError::Protocol(e.to_string()))?;
            line.push(b'\n');
            self.io.get_mut().write_all(&line).await?;
            self.io.get_mut().flush().await?;
            let mut reply = Vec::new();
            loop {
                let available = self.io.fill_buf().await?;
                if available.is_empty() {
                    return Err(CleanupError::Remote { code:"helper_unavailable".into(), message:"recovery helper ended without a result; verify Python 3, command execution and process inspection permissions on the server".into() });
                }
                let count = available.iter().position(|b| *b == b'\n').map_or(available.len(), |pos| pos+1);
                if reply.len()+count > MAX_REPLY { return Err(CleanupError::Protocol("helper response exceeds 32 KiB".into())); }
                reply.extend_from_slice(&available[..count]);
                self.io.consume(count);
                if reply.last() == Some(&b'\n') { break; }
            }
            let value: Value = serde_json::from_slice(&reply).map_err(|e| CleanupError::Protocol(format!("invalid helper JSON: {e}")))?;
            if value.get("protocol").and_then(Value::as_u64) != Some(1) || value.get("op").and_then(Value::as_str) != Some(op) {
                return Err(CleanupError::Protocol("unexpected helper protocol/operation".into()));
            }
            if value.get("ok").and_then(Value::as_bool) != Some(true) {
                return Err(CleanupError::Remote {
                    code:value.get("code").and_then(Value::as_str).unwrap_or("unknown").into(),
                    message:value.get("message").and_then(Value::as_str).unwrap_or("helper rejected operation").into(),
                });
            }
            Ok(value)
        }).await.map_err(|_| CleanupError::Timeout { operation:op })?
    }
}

struct OwnedChannel(Option<Channel<client::Msg>>);
impl OwnedChannel {
    fn channel(&self) -> &Channel<client::Msg> {
        self.0.as_ref().expect("channel owned until converted")
    }
    fn channel_mut(&mut self) -> &mut Channel<client::Msg> {
        self.0.as_mut().expect("channel owned until converted")
    }
    fn take(&mut self) -> Channel<client::Msg> {
        self.0.take().expect("channel consumed once")
    }
}
impl Drop for OwnedChannel {
    fn drop(&mut self) {
        if let Some(channel) = self.0.take() {
            drop(channel.into_stream());
        }
    }
}

async fn open_session<H>(
    handle: Arc<client::Handle<H>>,
    timeout: Duration,
) -> Result<Channel<client::Msg>, CleanupError>
where
    H: client::Handler + Send + 'static,
{
    let (sender, receiver) = tokio::sync::oneshot::channel();
    // Cancelling the caller does not abandon the channel-open request. A late
    // confirmation is converted to a close-on-drop stream if nobody receives it.
    tokio::spawn(async move {
        let result = handle.channel_open_session().await;
        if let Err(Ok(channel)) = sender.send(result) {
            drop(channel.into_stream());
        }
    });
    tokio::time::timeout(timeout, receiver)
        .await
        .map_err(|_| CleanupError::Timeout {
            operation: "session channel open",
        })?
        .map_err(|_| CleanupError::Protocol("session channel task ended".into()))?
        .map_err(CleanupError::from)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn embedded_program_is_one_shell_argument() {
        assert_eq!(shell_quote("a'b\n$HOME"), "'a'\"'\"'b\n$HOME'");
    }
}
