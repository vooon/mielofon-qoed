//! Length-prefixed JSON frames: a big-endian `u32` length, then the body.

use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest frame accepted (a full gossip push is the biggest message).
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {0} bytes exceeds the {MAX_FRAME} byte limit")]
    TooLarge(usize),
    #[error("stream closed before a frame arrived")]
    Closed,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Write one frame. Does not flush or close the stream.
pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(
    w: &mut W,
    value: &T,
) -> Result<(), FrameError> {
    let body = serde_json::to_vec(value)?;
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(body.len()));
    }
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(&body).await?;
    Ok(())
}

/// Read one frame. `Closed` when the stream ends cleanly before a header.
pub async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(
    r: &mut R,
) -> Result<T, FrameError> {
    let mut head = [0u8; 4];
    match r.read_exact(&mut head).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(FrameError::Closed),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(head) as usize;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_and_bounds() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &serde_json::json!({"a": 1}))
            .await
            .unwrap();
        assert_eq!(&buf[..4], &[0, 0, 0, 7]);
        let mut cur = std::io::Cursor::new(buf);
        let v: serde_json::Value = read_frame(&mut cur).await.unwrap();
        assert_eq!(v["a"], 1);
        // Clean end of stream: Closed, not an io error.
        assert!(matches!(
            read_frame::<_, serde_json::Value>(&mut cur).await,
            Err(FrameError::Closed)
        ));
        // An absurd header is refused before any allocation.
        let mut big = std::io::Cursor::new(vec![0xff, 0xff, 0xff, 0xff]);
        assert!(matches!(
            read_frame::<_, serde_json::Value>(&mut big).await,
            Err(FrameError::TooLarge(_))
        ));
    }
}
