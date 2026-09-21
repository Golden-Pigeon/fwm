use std::{
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use russh::{client, keys::PublicKeyOrCertificate};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::{HostKeyInfo, ResolvedServer, SshError, auth::authenticate, resolve, verify_host_key};
use crate::model::{RetryPolicy, ServerProfile};

/// Connect/authenticate using native SSH, including independently verified
/// native direct-tcpip jump connections. A timeout bounds agent interaction,
/// key exchange, and authentication as well as TCP establishment.
pub async fn connect<H>(
    profile: &ServerProfile,
    policy: &RetryPolicy,
    handler: H,
) -> Result<client::Handle<H>, SshError>
where
    H: client::Handler<Error = SshError> + Send + 'static,
{
    let server = resolve(profile)?;
    let seconds = policy.connect_timeout_secs.max(1);
    let authenticating = Arc::new(AtomicBool::new(false));
    tokio::time::timeout(Duration::from_secs(seconds), async {
        let stream = build_transport(profile, &server, policy, &authenticating).await?;
        let mut handle = client::connect_stream(client_config(policy), stream, handler).await?;
        authenticating.store(true, Ordering::Relaxed);
        if let Err(error) = authenticate(&mut handle, &server).await {
            let _ = handle
                .disconnect(
                    russh::Disconnect::ByApplication,
                    "authentication failed",
                    "en",
                )
                .await;
            return Err(error);
        }
        Ok(handle)
    })
    .await
    .map_err(|_| timeout_error(seconds, &authenticating))?
}

/// Verify trust and authentication without opening forwarding listeners.
pub async fn check_connection(
    profile: &ServerProfile,
    policy: &RetryPolicy,
) -> Result<(), SshError> {
    let handler = JumpHandler {
        server: resolve(profile)?,
    };
    let handle = connect(profile, policy, handler).await?;
    handle
        .disconnect(
            russh::Disconnect::ByApplication,
            "connection diagnostic complete",
            "en",
        )
        .await?;
    Ok(())
}

/// Read the server key without sending credentials to the target. Any jump
/// hosts must already be trusted and authenticate normally.
pub async fn inspect_host_key(
    profile: &ServerProfile,
    policy: &RetryPolicy,
) -> Result<HostKeyInfo, SshError> {
    inspect_key_at(profile, policy, None).await
}

pub async fn inspect_hop_key(
    profile: &ServerProfile,
    policy: &RetryPolicy,
    hop: &str,
) -> Result<HostKeyInfo, SshError> {
    inspect_key_at(profile, policy, Some(hop)).await
}

async fn inspect_key_at(
    profile: &ServerProfile,
    policy: &RetryPolicy,
    hop: Option<&str>,
) -> Result<HostKeyInfo, SshError> {
    let server = resolve(profile)?;
    let mut hops = Vec::new();
    expand_hops(profile, &server, &mut Vec::new(), &mut hops)?;
    let server = if let Some(selector) = hop {
        let index = select_hop(&hops, selector)?;
        let selected = hops[index].clone();
        hops.truncate(index);
        selected
    } else {
        server
    };
    let seconds = policy.connect_timeout_secs.max(1);
    let authenticating = Arc::new(AtomicBool::new(false));
    tokio::time::timeout(Duration::from_secs(seconds), async {
        let stream = transport_through(profile, &hops, &server, policy, &authenticating).await?;
        let key = Arc::new(Mutex::new(None));
        let handler = InspectHandler {
            server,
            key: key.clone(),
        };
        let handle = client::connect_stream(client_config(policy), stream, handler).await?;
        let info = key
            .lock()
            .map_err(|_| SshError::Configuration("host inspection state poisoned".into()))?
            .take()
            .ok_or_else(|| SshError::Configuration("server did not supply a host key".into()))?;
        let _ = handle
            .disconnect(
                russh::Disconnect::ByApplication,
                "host key inspection complete",
                "en",
            )
            .await;
        Ok(info)
    })
    .await
    .map_err(|_| timeout_error(seconds, &authenticating))?
}

fn timeout_error(seconds: u64, authenticating: &AtomicBool) -> SshError {
    if authenticating.load(Ordering::Relaxed) {
        SshError::Authentication(format!(
            "authentication/agent did not finish within {seconds}s; unlock or authorize the key before retrying"
        ))
    } else {
        SshError::Timeout(seconds)
    }
}

fn client_config(policy: &RetryPolicy) -> Arc<client::Config> {
    Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(policy.keepalive_interval_secs.max(1))),
        keepalive_max: policy.keepalive_max.max(1),
        inactivity_timeout: None,
        nodelay: true,
        ..Default::default()
    })
}

trait Transport: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Transport for T {}
type BoxTransport = Box<dyn Transport>;

async fn build_transport(
    profile: &ServerProfile,
    target: &ResolvedServer,
    policy: &RetryPolicy,
    authenticating: &AtomicBool,
) -> Result<BoxTransport, SshError> {
    let mut hops = Vec::new();
    expand_hops(profile, target, &mut Vec::new(), &mut hops)?;
    transport_through(profile, &hops, target, policy, authenticating).await
}

async fn transport_through(
    profile: &ServerProfile,
    hops: &[ResolvedServer],
    target: &ResolvedServer,
    policy: &RetryPolicy,
    authenticating: &AtomicBool,
) -> Result<BoxTransport, SshError> {
    if hops.is_empty() {
        return tcp(&target.host, target.port).await;
    }
    let mut stream = tcp(&hops[0].host, hops[0].port)
        .await
        .map_err(|error| hop_error(profile, 0, &hops[0], error))?;
    for (index, hop) in hops.iter().enumerate() {
        let mut handle = client::connect_stream(
            client_config(policy),
            stream,
            JumpHandler {
                server: hop.clone(),
            },
        )
        .await
        .map_err(|error| hop_error(profile, index, hop, error))?;
        authenticating.store(true, Ordering::Relaxed);
        authenticate(&mut handle, hop)
            .await
            .map_err(|error| hop_error(profile, index, hop, error))?;
        authenticating.store(false, Ordering::Relaxed);
        let next = hops.get(index + 1).unwrap_or(target);
        let channel = handle
            .channel_open_direct_tcpip(&next.host, u32::from(next.port), "127.0.0.1", 0)
            .await
            .map_err(|error| hop_error(profile, index, hop, error.into()))?;
        stream = Box::new(JumpStream {
            stream: Box::new(channel.into_stream()),
            _session: handle,
        });
    }
    Ok(stream)
}

fn hop_error(
    profile: &ServerProfile,
    index: usize,
    hop: &ResolvedServer,
    error: SshError,
) -> SshError {
    let remedy = if matches!(
        &error,
        SshError::UnknownHostKey { .. }
            | SshError::HostKeyChanged { .. }
            | SshError::RevokedHostKey(_)
    ) {
        format!(
            "inspect/trust this hop with `fwm server trust {} --hop {}` using the same --config-dir and --ssh-config",
            profile.name,
            index + 1
        )
    } else {
        format!(
            "check this jump host's connection, authentication and forwarding settings in {} before retrying the target",
            hop.ssh_config.display()
        )
    };
    SshError::Hop {
        index: index + 1,
        alias: hop.alias.clone(),
        host: hop.host.clone(),
        port: hop.port,
        known_hosts: hop.known_hosts.clone(),
        remedy,
        error: Box::new(error),
    }
}

fn select_hop(hops: &[ResolvedServer], selector: &str) -> Result<usize, SshError> {
    if let Ok(number) = selector.parse::<usize>()
        && number > 0
        && number <= hops.len()
    {
        return Ok(number - 1);
    }
    let matching: Vec<_> = hops
        .iter()
        .enumerate()
        .filter(|(_, hop)| hop.alias == selector)
        .map(|(index, _)| index)
        .collect();
    match matching.as_slice() {
        [index] => Ok(*index),
        [] => Err(SshError::Configuration(format!(
            "unknown SSH hop {selector:?}; available hops: {}",
            hops.iter()
                .enumerate()
                .map(|(i, h)| format!("{}:{}", i + 1, h.alias))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
        _ => Err(SshError::Configuration(format!(
            "SSH hop alias {selector:?} is ambiguous; choose its 1-based hop number"
        ))),
    }
}

pub fn resolved_route(profile: &ServerProfile) -> Result<Vec<ResolvedServer>, SshError> {
    let target = resolve(profile)?;
    let mut hops = Vec::new();
    expand_hops(profile, &target, &mut Vec::new(), &mut hops)?;
    hops.push(target);
    Ok(hops)
}

async fn tcp(host: &str, port: u16) -> Result<BoxTransport, SshError> {
    let stream = tokio::net::TcpStream::connect((host, port)).await?;
    stream.set_nodelay(true)?;
    Ok(Box::new(stream))
}

fn expand_hops(
    profile: &ServerProfile,
    target: &ResolvedServer,
    ancestors: &mut Vec<String>,
    output: &mut Vec<ResolvedServer>,
) -> Result<(), SshError> {
    let identity = format!("{}@{}:{}", target.user, target.host, target.port);
    if ancestors.contains(&identity) || ancestors.len() >= 16 || output.len() >= 16 {
        return Err(SshError::Configuration(
            "ProxyJump chain contains a cycle or exceeds 16 hops".into(),
        ));
    }
    ancestors.push(identity);
    for jump in &target.proxy_jump {
        let mut jump_profile = parse_jump(jump)?;
        jump_profile.ssh_config = profile.ssh_config.clone();
        // An explicitly selected trust database belongs to the entire profile
        // (including its jumps); otherwise each alias resolves its own config.
        jump_profile.known_hosts = profile.known_hosts.clone();
        let resolved = resolve(&jump_profile)?;
        expand_hops(&jump_profile, &resolved, ancestors, output)?;
        output.push(resolved);
    }
    ancestors.pop();
    Ok(())
}

fn parse_jump(value: &str) -> Result<ServerProfile, SshError> {
    if value.is_empty() || value.contains([' ', '\t', '\n', '\r']) {
        return Err(SshError::Configuration("invalid ProxyJump entry".into()));
    }
    let (user, address) = if let Some((user, address)) = value.rsplit_once('@') {
        (Some(user.to_string()), address)
    } else {
        (None, value)
    };
    let (alias, port) = if let Some(bracketed) = address.strip_prefix('[') {
        let (host, suffix) = bracketed
            .split_once(']')
            .ok_or_else(|| SshError::Configuration("unterminated IPv6 ProxyJump address".into()))?;
        let port = if suffix.is_empty() {
            None
        } else {
            Some(
                suffix
                    .strip_prefix(':')
                    .ok_or_else(|| SshError::Configuration("invalid ProxyJump port".into()))?
                    .parse()
                    .map_err(|_| SshError::Configuration("invalid ProxyJump port".into()))?,
            )
        };
        (host, port)
    } else if let Some((host, port)) = address.rsplit_once(':') {
        if host.contains(':') {
            return Err(SshError::Configuration(
                "IPv6 ProxyJump hosts need brackets".into(),
            ));
        }
        (
            host,
            Some(
                port.parse()
                    .map_err(|_| SshError::Configuration("invalid ProxyJump port".into()))?,
            ),
        )
    } else {
        (address, None)
    };
    let mut profile = ServerProfile::new(value);
    profile.ssh_alias = Some(alias.to_owned());
    profile.user = user;
    profile.port = port;
    Ok(profile)
}

struct JumpHandler {
    server: ResolvedServer,
}

impl client::Handler for JumpHandler {
    type Error = SshError;
    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        verify_host_key(&self.server, key)
    }
}

struct InspectHandler {
    server: ResolvedServer,
    key: Arc<Mutex<Option<HostKeyInfo>>>,
}

impl client::Handler for InspectHandler {
    type Error = SshError;
    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            return Err(SshError::HostCertificateUnsupported);
        };
        let info = HostKeyInfo::from_key(&self.server, key)?;
        *self
            .key
            .lock()
            .map_err(|_| SshError::Configuration("host inspection state poisoned".into()))? =
            Some(info);
        // Inspection completes only the transport handshake, never userauth.
        Ok(true)
    }
}

/// Retaining the hop's handle is essential: dropping it would close the
/// session's control sender. Nested streams retain the entire jump chain.
struct JumpStream {
    stream: BoxTransport,
    _session: client::Handle<JumpHandler>,
}

impl AsyncRead for JumpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for JumpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn jump_parsing_keeps_ipv6_and_user() {
        let profile = parse_jump("alice@[::1]:2222").unwrap();
        assert_eq!(profile.user.as_deref(), Some("alice"));
        assert_eq!(profile.ssh_alias.as_deref(), Some("::1"));
        assert_eq!(profile.port, Some(2222));
        assert!(parse_jump("::1:2222").is_err());
    }
}
