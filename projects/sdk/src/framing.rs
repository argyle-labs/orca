//! Length-prefixed framing for JSON-RPC messages over a stream transport.
//!
//! Wire format: 4-byte big-endian u32 length followed by exactly that many UTF-8 bytes.

use anyhow::{Result, ensure};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum frame body size (16 MiB). Guards against malformed/malicious peers.
const MAX_FRAME: u32 = 16 * 1024 * 1024;

/// Write one framed message: length header + body.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, body: &[u8]) -> Result<()> {
    let len = u32::try_from(body.len())
        .map_err(|_| anyhow::anyhow!("message too large to frame: {} bytes", body.len()))?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(body).await?;
    Ok(())
}

/// Read one framed message: consume 4-byte length header, then that many bytes.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>> {
    let mut hdr = [0u8; 4];
    r.read_exact(&mut hdr).await?;
    let len = u32::from_be_bytes(hdr);
    ensure!(len <= MAX_FRAME, "frame too large: {len} bytes");
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip_small_message() {
        let msg = b"hello world";
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, msg).await.unwrap();
        // buf = [0,0,0,11] ++ "hello world"
        assert_eq!(buf.len(), 4 + msg.len());
        let got = read_frame(&mut buf.as_slice()).await.unwrap();
        assert_eq!(got, msg);
    }

    #[tokio::test]
    async fn roundtrip_empty_message() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, b"").await.unwrap();
        let got = read_frame(&mut buf.as_slice()).await.unwrap();
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn rejects_oversized_frame() {
        // Craft a header claiming MAX_FRAME + 1 bytes
        let big = (MAX_FRAME + 1).to_be_bytes();
        let err = read_frame(&mut big.as_slice()).await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("frame too large"));
    }
}
