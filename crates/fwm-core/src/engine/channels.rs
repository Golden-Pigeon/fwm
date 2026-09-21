//! Ownership of channel-open requests outlives a cancelled application connection.
use std::{net::SocketAddr, time::Duration};

use russh::{ChannelStream, client};
use tokio::sync::{OwnedSemaphorePermit, oneshot};

use super::connection::SshHandle;
use crate::model::Endpoint;

pub(super) type Opened = Result<(ChannelStream<client::Msg>, OwnedSemaphorePermit), russh::Error>;

/// russh's raw Channel does not close on drop, and abandoning its open future
/// does not cancel a pending SSH request. Keep the request and its capacity slot
/// until confirmation; convert immediately to a stream that closes on drop.
pub(super) fn open_direct(
    handle: SshHandle,
    target: Endpoint,
    originator: SocketAddr,
    permit: OwnedSemaphorePermit,
) -> oneshot::Receiver<Opened> {
    let (sender, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let opened = handle.channel_open_direct_tcpip(
            target.host,
            u32::from(target.port),
            originator.ip().to_string(),
            u32::from(originator.port()),
        );
        tokio::pin!(opened);
        let mut check = tokio::time::interval(Duration::from_millis(250));
        let result = loop {
            tokio::select! {
                result = &mut opened => break result,
                _ = check.tick() => if handle.is_closed() { return; },
            }
        };
        // If the local client timed out or was cancelled, send fails and drops
        // this stream, issuing CLOSE for a successfully opened late channel.
        let _ = sender.send(result.map(|channel| (channel.into_stream(), permit)));
    });
    receiver
}
