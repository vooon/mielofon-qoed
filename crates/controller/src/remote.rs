//! Minimal mTLS HTTPS client for gossip anti-entropy pushes. Written against
//! tokio-rustls (ring) directly so we keep the provider consistent and avoid
//! pulling reqwest/aws-lc.

use anyhow::{bail, Context};
use rustls::pki_types::ServerName;
use rustls::ClientConfig;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

/// Largest response body accepted from a peer (a full KV view + samples).
const MAX_BODY: usize = 16 * 1024 * 1024;

/// POST `path` with `body` to `peer:port` over mTLS and return the response
/// body. Connects by IP (the peer's advertise address). The whole exchange
/// (connect, handshake, request, response) is bounded by `timeout`, so a
/// black-holed peer cannot stall the caller for the kernel's connect timeout.
pub async fn post(
    peer: IpAddr,
    port: u16,
    client: Arc<ClientConfig>,
    path: &str,
    body: &[u8],
    timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    tokio::time::timeout(timeout, exchange(peer, port, client, path, body))
        .await
        .map_err(|_| anyhow::anyhow!("timed out after {}s", timeout.as_secs()))?
}

async fn exchange(
    peer: IpAddr,
    port: u16,
    client: Arc<ClientConfig>,
    path: &str,
    body: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let tcp = TcpStream::connect((peer, port)).await.context("connect")?;
    let connector = TlsConnector::from(client);
    let server_name = ServerName::IpAddress(peer.into());
    let mut tls = connector
        .connect(server_name, tcp)
        .await
        .context("tls handshake")?;

    let host = peer.to_string();
    let header = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    tls.write_all(header.as_bytes()).await?;
    tls.write_all(body).await?;
    tls.flush().await?;

    // `Connection: close`: the response ends with the stream.
    let mut raw = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match tls.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > MAX_BODY {
                    bail!("response larger than {MAX_BODY} bytes");
                }
            }
            // rustls reports a missing close_notify as an error after the
            // peer has already closed the TCP side; what we have is complete.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e).context("read response"),
        }
    }
    parse_response(&raw)
}

/// Split an HTTP/1.1 response into status + body, de-chunking when needed.
fn parse_response(raw: &[u8]) -> anyhow::Result<Vec<u8>> {
    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        bail!("malformed response (no header terminator)");
    };
    let head = String::from_utf8_lossy(&raw[..split]);
    let body = &raw[split + 4..];
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .context("malformed status line")?;
    if !(200..300).contains(&status) {
        bail!("HTTP {status}");
    }
    let chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    if chunked {
        dechunk(body)
    } else {
        Ok(body.to_vec())
    }
}

fn dechunk(mut body: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let Some(eol) = body.windows(2).position(|w| w == b"\r\n") else {
            bail!("malformed chunk header");
        };
        let size_line = String::from_utf8_lossy(&body[..eol]);
        let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
            .context("malformed chunk size")?;
        body = &body[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        if body.len() < size + 2 {
            bail!("truncated chunk");
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_chunked_bodies() {
        let plain = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        assert_eq!(parse_response(plain).unwrap(), b"{}");
        let chunked =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n{\"a\r\n2\r\n\"}\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked).unwrap(), b"{\"a\"}");
        let err = parse_response(b"HTTP/1.1 500 Internal\r\n\r\nx").unwrap_err();
        assert!(err.to_string().contains("500"));
        assert!(parse_response(b"garbage").is_err());
    }
}
