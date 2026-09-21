use crate::protocol::MAX_FRAME_BYTES;
use serde::{Serialize, de::DeserializeOwned};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub async fn read_frame<T: DeserializeOwned, R: AsyncRead + Unpin + ?Sized>(
    reader: &mut R,
) -> io::Result<Option<T>> {
    let mut length = [0u8; 4];
    match reader.read(&mut length[..1]).await? {
        0 => return Ok(None),
        _ => reader.read_exact(&mut length[1..]).await?,
    };
    let size = u32::from_be_bytes(length) as usize;
    if size == 0 || size > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid or oversized IPC frame",
        ));
    }
    let mut payload = vec![0; size];
    reader.read_exact(&mut payload).await?;
    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub async fn write_frame<T: Serialize, W: AsyncWrite + Unpin + ?Sized>(
    writer: &mut W,
    value: &T,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC response exceeds frame limit",
        ));
    }
    writer
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .await?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn rejects_oversize_before_allocating_payload() {
        let mut bytes = (MAX_FRAME_BYTES as u32 + 1)
            .to_be_bytes()
            .as_slice()
            .to_vec();
        assert_eq!(
            read_frame::<serde_json::Value, _>(&mut bytes.as_slice())
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        bytes.clear();
        assert!(
            read_frame::<serde_json::Value, _>(&mut bytes.as_slice())
                .await
                .unwrap()
                .is_none()
        );
    }
    #[tokio::test]
    async fn roundtrip_through_fragmented_transport() {
        let (mut a, mut b) = tokio::io::duplex(8);
        let value = serde_json::json!({"method":"status", "unicode":"转发"});
        let expected = value.clone();
        let writer = tokio::spawn(async move {
            write_frame(&mut a, &value).await.unwrap();
        });
        assert_eq!(
            read_frame::<serde_json::Value, _>(&mut b)
                .await
                .unwrap()
                .unwrap(),
            expected
        );
        writer.await.unwrap();
    }
}
