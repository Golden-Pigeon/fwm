use super::{CleanupContext, CleanupError, context::Claim};
use crate::model::ForwardSpec;
use russh::{Channel, ChannelMsg, ChannelStream, client};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

// Native helper artifacts are uploaded over the dedicated SSH connection.
// Keeping the bytes in the client means the server needs no Python, compiler,
// package manager, or matching user-space runtime.
const HELPER_LINUX_X86_64: &[u8] = include_bytes!("native_helper_linux_x86_64");
const HELPER_MACOS_UNIVERSAL: &[u8] = include_bytes!("native_helper_macos_universal");
const HELPER_WINDOWS_X86_64: &[u8] = include_bytes!("native_helper_windows_x86_64.exe");
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
        let channel = open_session(handle, &claim, timeout).await?;
        let mut owned = OwnedChannel(Some(channel));
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
                    return Err(CleanupError::Remote { code:"helper_unavailable".into(), message:"native recovery helper ended without a result; verify command execution and native process-inspection permissions on the server".into() });
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

#[derive(Clone, Copy)]
enum RemotePlatform {
    Linux,
    Macos,
    Windows,
}

struct CommandResult {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_status: u32,
}

/// SSH EOF ends output, not channel requests. In particular OpenSSH may send
/// exit-status after EOF. Keep collecting until channel close, so neither an
/// early EOF nor an early exit-status loses the command's result or output.
async fn command_result(
    channel: &mut OwnedChannel,
    timeout: Duration,
    operation: &'static str,
) -> Result<CommandResult, CleanupError> {
    tokio::time::timeout(timeout, async {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_status = None;
        loop {
            match channel.channel_mut().wait().await {
                Some(ChannelMsg::Data { data }) => {
                    if stdout.len() + stderr.len() + data.len() > MAX_REPLY {
                        return Err(CleanupError::Protocol(format!(
                            "{operation} output exceeds 32 KiB"
                        )));
                    }
                    stdout.extend_from_slice(&data);
                }
                Some(ChannelMsg::ExtendedData { data, .. }) => {
                    if stdout.len() + stderr.len() + data.len() > MAX_REPLY {
                        return Err(CleanupError::Protocol(format!(
                            "{operation} output exceeds 32 KiB"
                        )));
                    }
                    stderr.extend_from_slice(&data);
                }
                Some(ChannelMsg::ExitStatus {
                    exit_status: status,
                }) => {
                    exit_status = Some(status);
                }
                Some(ChannelMsg::Failure) => {
                    return Err(CleanupError::Remote {
                        code: "exec_denied".into(),
                        message: format!("SSH server rejected {operation}"),
                    });
                }
                Some(ChannelMsg::ExitSignal { .. }) => {
                    return Err(CleanupError::Remote {
                        code: "helper_unavailable".into(),
                        message: format!("{operation} was terminated by a signal"),
                    });
                }
                Some(ChannelMsg::Close) | None => {
                    return Ok(CommandResult {
                        stdout,
                        stderr,
                        exit_status: exit_status.ok_or_else(|| {
                            CleanupError::Protocol(format!(
                                "{operation} closed without an exit status"
                            ))
                        })?,
                    });
                }
                // EOF carries no success/failure information. Wait for the
                // exit-status request and the eventual channel close.
                Some(ChannelMsg::Eof) => {}
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| CleanupError::Timeout { operation })?
}

async fn probe_remote_command<H>(
    handle: Arc<client::Handle<H>>,
    command: &str,
    timeout: Duration,
) -> Result<(bool, String), CleanupError>
where
    H: client::Handler + Send + 'static,
{
    let channel = open_session_raw(handle, timeout).await?;
    let mut owned = OwnedChannel(Some(channel));
    owned
        .channel()
        .exec(true, command)
        .await
        .map_err(|_| CleanupError::Remote {
            code: "exec_denied".into(),
            message: "SSH server rejected remote platform detection".into(),
        })?;
    let result = command_result(&mut owned, timeout, "remote platform detection").await?;
    Ok((
        result.exit_status == 0,
        String::from_utf8_lossy(&result.stdout).into_owned(),
    ))
}

async fn detect_remote_platform<H>(
    handle: Arc<client::Handle<H>>,
    timeout: Duration,
) -> Result<RemotePlatform, CleanupError>
where
    H: client::Handler + Send + 'static,
{
    let (ok, uname) = probe_remote_command(handle.clone(), "uname -s -m", timeout).await?;
    let uname = uname.trim();
    if ok && uname.starts_with("Linux ") {
        if uname.split_whitespace().nth(1) == Some("x86_64") {
            return Ok(RemotePlatform::Linux);
        }
        return Err(CleanupError::Remote {
            code: "unsupported".into(),
            message: format!(
                "native recovery helper does not support remote Linux architecture: {uname}"
            ),
        });
    }
    if ok && uname.starts_with("Darwin ") {
        return Ok(RemotePlatform::Macos);
    }
    let (ok, windows) =
        probe_remote_command(handle, "echo %OS% %PROCESSOR_ARCHITECTURE%", timeout).await?;
    let windows = windows.trim();
    if ok && windows.starts_with("Windows_NT ") && windows.ends_with("AMD64") {
        return Ok(RemotePlatform::Windows);
    }
    Err(CleanupError::Remote {
        code: "unsupported".into(),
        message: format!(
            "cannot identify a supported remote platform (uname={uname:?}, platform={windows:?})"
        ),
    })
}

async fn open_session<H>(
    handle: Arc<client::Handle<H>>,
    claim: &Claim,
    timeout: Duration,
) -> Result<Channel<client::Msg>, CleanupError>
where
    H: client::Handler + Send + 'static,
{
    tokio::time::timeout(timeout, open_session_unbounded(handle, claim, timeout))
        .await
        .map_err(|_| CleanupError::Timeout {
            operation: "helper startup",
        })?
}

async fn open_session_unbounded<H>(
    handle: Arc<client::Handle<H>>,
    claim: &Claim,
    timeout: Duration,
) -> Result<Channel<client::Msg>, CleanupError>
where
    H: client::Handler + Send + 'static,
{
    let platform = detect_remote_platform(handle.clone(), timeout).await?;
    let helper_name = format!("fwm-remote-helper-{}", claim.session_id);
    let (helper_bytes, upload_command, exec_command) = match platform {
        RemotePlatform::Linux => (
            HELPER_LINUX_X86_64,
            format!(
                "umask 077; set -C; cat > {} && chmod 700 {}",
                shell_quote(&format!("/tmp/.{helper_name}")),
                shell_quote(&format!("/tmp/.{helper_name}"))
            ),
            format!("exec {}", shell_quote(&format!("/tmp/.{helper_name}"))),
        ),
        RemotePlatform::Macos => (
            HELPER_MACOS_UNIVERSAL,
            format!(
                "umask 077; set -C; cat > {} && chmod 700 {}",
                shell_quote(&format!("/tmp/.{helper_name}")),
                shell_quote(&format!("/tmp/.{helper_name}"))
            ),
            format!("exec {}", shell_quote(&format!("/tmp/.{helper_name}"))),
        ),
        RemotePlatform::Windows => {
            let remote_name = format!("{helper_name}.exe");
            let upload = format!(
                "powershell -NoProfile -NonInteractive -Command \"$p=Join-Path $env:TEMP '{remote_name}'; $o=[IO.File]::Open($p,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None); [Console]::OpenStandardInput().CopyTo($o); $o.Close()\""
            );
            let exec = format!(
                "powershell -NoProfile -NonInteractive -Command \"& (Join-Path $env:TEMP '{remote_name}')\""
            );
            (HELPER_WINDOWS_X86_64, upload, exec)
        }
    };
    let upload = open_session_raw(handle.clone(), timeout).await?;
    let mut upload = OwnedChannel(Some(upload));
    upload
        .channel()
        .exec(true, upload_command)
        .await
        .map_err(|_| CleanupError::Remote {
            code: "exec_denied".into(),
            message: "SSH server rejected the native helper upload command".into(),
        })?;
    let mut startup_stderr = Vec::new();
    loop {
        match upload.channel_mut().wait().await {
            Some(ChannelMsg::Success) => break,
            Some(ChannelMsg::Failure) => {
                return Err(CleanupError::Remote {
                    code: "exec_denied".into(),
                    message: "SSH server rejected the native helper upload command".into(),
                });
            }
            Some(ChannelMsg::ExtendedData { data, .. }) => {
                if startup_stderr.len() + data.len() > MAX_REPLY {
                    return Err(CleanupError::Protocol(
                        "helper upload startup output exceeds 32 KiB".into(),
                    ));
                }
                startup_stderr.extend_from_slice(&data);
            }
            Some(ChannelMsg::ExitStatus { exit_status }) => {
                return Err(CleanupError::Remote {
                    code: "helper_upload_failed".into(),
                    message: format!(
                        "helper upload command exited with status {exit_status}: {}",
                        String::from_utf8_lossy(&startup_stderr).trim()
                    ),
                });
            }
            Some(ChannelMsg::Close) | None => {
                return Err(CleanupError::Remote {
                    code: "helper_upload_failed".into(),
                    message: format!(
                        "helper upload channel closed before exec confirmation: {}",
                        String::from_utf8_lossy(&startup_stderr).trim()
                    ),
                });
            }
            Some(ChannelMsg::Eof) => {}
            _ => {}
        }
    }
    upload.channel().data_bytes(helper_bytes.to_vec()).await?;
    upload.channel().eof().await?;
    let result = command_result(&mut upload, timeout, "helper upload").await?;
    if result.exit_status != 0 {
        return Err(CleanupError::Remote {
            code: "helper_upload_failed".into(),
            message: format!(
                "native helper upload exited with status {}: {}",
                result.exit_status,
                String::from_utf8_lossy(&result.stderr).trim()
            ),
        });
    }
    drop(upload);

    let channel = open_session_raw(handle, timeout).await?;
    let mut owned = OwnedChannel(Some(channel));
    owned
        .channel()
        .exec(true, exec_command)
        .await
        .map_err(|_| CleanupError::Remote {
            code: "exec_denied".into(),
            message: "SSH server rejected the native recovery helper".into(),
        })?;
    let mut stderr = Vec::new();
    loop {
        match owned.channel_mut().wait().await {
            Some(ChannelMsg::Success) => return Ok(owned.take()),
            Some(ChannelMsg::Failure) => return Err(CleanupError::Remote { code: "exec_denied".into(), message: "SSH server rejected the native recovery helper; allow command execution for verified recovery".into() }),
            Some(ChannelMsg::ExtendedData { data, .. }) => {
                if stderr.len() + data.len() > MAX_REPLY {
                    return Err(CleanupError::Protocol("helper startup output exceeds 32 KiB".into()));
                }
                stderr.extend_from_slice(&data);
            }
            Some(ChannelMsg::ExitStatus { exit_status }) => return Err(CleanupError::Remote { code: "helper_unavailable".into(), message: format!("native recovery helper exited with status {exit_status}: {}", String::from_utf8_lossy(&stderr).trim()) }),
            Some(ChannelMsg::Close) | None => return Err(CleanupError::Remote { code: "helper_unavailable".into(), message: format!("native recovery helper channel closed before startup: {}", String::from_utf8_lossy(&stderr).trim()) }),
            Some(ChannelMsg::Eof) => {},
            _ => {}
        }
    }
}

async fn open_session_raw<H>(
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
    fn shell_quote_preserves_one_shell_argument() {
        assert_eq!(shell_quote("a'b\n$HOME"), "'a'\"'\"'b\n$HOME'");
    }
}
