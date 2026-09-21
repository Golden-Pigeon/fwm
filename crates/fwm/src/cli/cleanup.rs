//! Resolve CLI cleanup defaults consistently for add and direction-changing edits.
use super::args::{CleanupMode, Mode};
use anyhow::{Result, bail};
use fwm_core::model::{ConnectionMode, ForwardSpec, RemoteCleanup, Tunnel};

pub(super) fn effective(
    tunnel: &Tunnel,
    existing: Option<&ForwardSpec>,
    requested_mode: Option<Mode>,
    requested_cleanup: Option<CleanupMode>,
) -> Result<(ConnectionMode, RemoteCleanup)> {
    let remote_cleanup = match requested_cleanup {
        Some(CleanupMode::Verified) if !tunnel.is_remote() => {
            bail!(
                "--remote-cleanup verified requires a remote forward; use --remote, --remote-dynamic or --remote-cleanup off"
            );
        }
        Some(CleanupMode::Verified) => RemoteCleanup::Verified,
        Some(CleanupMode::Off) => RemoteCleanup::Off,
        None if !tunnel.is_remote() => RemoteCleanup::Off,
        None => existing
            .filter(|forward| forward.tunnel.is_remote())
            .map_or(RemoteCleanup::Verified, |forward| forward.remote_cleanup),
    };
    let connection_mode = if remote_cleanup == RemoteCleanup::Verified {
        ConnectionMode::Dedicated
    } else {
        requested_mode.map(Into::into).unwrap_or_else(|| {
            existing.map_or(ConnectionMode::Shared, |forward| forward.connection_mode)
        })
    };
    Ok((connection_mode, remote_cleanup))
}

pub(super) fn describe(forward: &ForwardSpec) {
    if !forward.tunnel.is_remote() {
        return;
    }
    let cleanup = match forward.remote_cleanup {
        RemoteCleanup::Verified => "verified",
        RemoteCleanup::Off => "off",
    };
    let mode = match forward.connection_mode {
        ConnectionMode::Dedicated => "dedicated",
        ConnectionMode::Shared => "shared",
    };
    println!("    Remote cleanup: {cleanup}; effective connection mode: {mode}");
}
