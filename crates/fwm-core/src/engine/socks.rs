use std::net::{Ipv4Addr, Ipv6Addr};

use anyhow::{Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::model::Endpoint;

/// SOCKS5 CONNECT preserving hostnames for the caller's destination connection.
/// The caller imposes a handshake timeout.
pub(super) async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
) -> Result<Endpoint> {
    if stream.read_u8().await? != 5 {
        bail!("only SOCKS5 is supported");
    }
    let count = stream.read_u8().await? as usize;
    let mut methods = vec![0; count];
    stream.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        stream.write_all(&[5, 255]).await?;
        bail!("SOCKS5 client must support no-authentication mode");
    }
    stream.write_all(&[5, 0]).await?;
    let mut header = [0; 4];
    stream.read_exact(&mut header).await?;
    if header[0] != 5 || header[2] != 0 {
        reply(stream, 1).await?;
        bail!("invalid SOCKS5 request");
    }
    if header[1] != 1 {
        reply(stream, 7).await?;
        bail!("SOCKS5 only supports CONNECT");
    }
    let host = match header[3] {
        1 => {
            let mut address = [0; 4];
            stream.read_exact(&mut address).await?;
            Ipv4Addr::from(address).to_string()
        }
        3 => {
            let count = stream.read_u8().await? as usize;
            if count == 0 {
                reply(stream, 8).await?;
                bail!("empty SOCKS5 hostname");
            }
            let mut address = vec![0; count];
            stream.read_exact(&mut address).await?;
            match String::from_utf8(address) {
                Ok(host) => host,
                Err(error) => {
                    reply(stream, 8).await?;
                    return Err(error.into());
                }
            }
        }
        4 => {
            let mut address = [0; 16];
            stream.read_exact(&mut address).await?;
            Ipv6Addr::from(address).to_string()
        }
        _ => {
            reply(stream, 8).await?;
            bail!("unsupported SOCKS5 address type");
        }
    };
    let port = stream.read_u16().await?;
    if port == 0 || host.contains('\0') {
        reply(stream, 1).await?;
        bail!("invalid SOCKS5 destination");
    }
    Ok(Endpoint { host, port })
}

pub(super) async fn reply<S: AsyncWrite + Unpin>(stream: &mut S, status: u8) -> Result<()> {
    stream
        .write_all(&[5, status, 0, 1, 0, 0, 0, 0, 0, 0])
        .await?;
    Ok(())
}

#[cfg(test)]
#[path = "socks_protocol_tests.rs"]
mod protocol_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn preserves_hostname_for_remote_dns() {
        let (mut client, mut server) = duplex(128);
        let request = tokio::spawn(async move { handshake(&mut server).await });
        client.write_all(&[5, 1, 0]).await.unwrap();
        let mut selected = [0; 2];
        client.read_exact(&mut selected).await.unwrap();
        assert_eq!(selected, [5, 0]);
        client.write_all(&[5, 1, 0, 3, 11]).await.unwrap();
        client.write_all(b"example.org").await.unwrap();
        client.write_all(&443_u16.to_be_bytes()).await.unwrap();
        let target = request.await.unwrap().unwrap();
        assert_eq!(target.host, "example.org");
        assert_eq!(target.port, 443);
    }

    #[tokio::test]
    async fn rejects_udp_with_protocol_reply() {
        let (mut client, mut server) = duplex(128);
        let request = tokio::spawn(async move { handshake(&mut server).await });
        client.write_all(&[5, 1, 0, 5, 3, 0, 1]).await.unwrap();
        let mut responses = [0; 12];
        client.read_exact(&mut responses).await.unwrap();
        assert_eq!(&responses[..2], &[5, 0]);
        assert_eq!(&responses[2..4], &[5, 7]);
        assert!(request.await.unwrap().is_err());
    }
}
