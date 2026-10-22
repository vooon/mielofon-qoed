//! The carrier-independent session: open or accept bidirectional streams,
//! and the one-request-per-stream exchange built on them.

use crate::frame::{read_frame, write_frame, FrameError};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One bidirectional stream. Closing `send` (shutdown) tells the peer the
/// request or response is complete.
pub struct BiStream {
    pub send: Box<dyn AsyncWrite + Send + Unpin>,
    pub recv: Box<dyn AsyncRead + Send + Unpin>,
}

/// A live session with a peer. Both sides can open streams; `accept` yields
/// the ones the peer opened. Every method fails once the session is closed.
pub trait Session: Send + Sync {
    /// Open a stream towards the peer.
    fn open(&self) -> BoxFuture<'_, std::io::Result<BiStream>>;
    /// Wait for a stream the peer opened. `Err` once the session is gone.
    fn accept(&self) -> BoxFuture<'_, std::io::Result<BiStream>>;
    /// Resolves with the reason once the session has closed, either way.
    fn closed(&self) -> BoxFuture<'_, String>;
    /// Close the session with a reason the peer can log.
    fn close(&self, reason: &str);
    /// The peer's underlay address.
    fn remote(&self) -> SocketAddr;
}

fn io_err(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// Open a stream, send one request frame, close the send side, read one
/// response frame. The whole exchange is bounded by `timeout`.
pub async fn request<Q: Serialize + Sync, R: DeserializeOwned>(
    session: &dyn Session,
    req: &Q,
    timeout: Duration,
) -> std::io::Result<R> {
    let exchange = async {
        let mut s = session.open().await?;
        write_frame(&mut s.send, req).await.map_err(io_err)?;
        s.send.shutdown().await?;
        read_frame(&mut s.recv).await.map_err(|e| match e {
            FrameError::Io(e) => e,
            other => io_err(other),
        })
    };
    tokio::time::timeout(timeout, exchange)
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "request timed out"))?
}

/// Serve one accepted stream: read the request frame, let `handle` produce
/// the response, write it and close. `None` from `handle` closes the stream
/// without a response (the peer sees a clean end).
pub async fn respond<Q, R, F, Fut>(mut stream: BiStream, handle: F) -> std::io::Result<()>
where
    Q: DeserializeOwned,
    R: Serialize,
    F: FnOnce(Q) -> Fut,
    Fut: Future<Output = Option<R>>,
{
    let req: Q = read_frame(&mut stream.recv).await.map_err(io_err)?;
    if let Some(resp) = handle(req).await {
        write_frame(&mut stream.send, &resp).await.map_err(io_err)?;
    }
    stream.send.shutdown().await
}
