//! Minimal mTLS HTTPS client for gossip anti-entropy pushes. Written against
//! tokio-rustls (ring) directly so we keep the provider consistent and avoid
//! pulling reqwest/aws-lc.

use rustls::pki_types::ServerName;
use rustls::ClientConfig;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

/// POST `path` with `body` to `peer:port` over mTLS. Connects by IP (the peer's
/// advertise address) and discards the response body on success.
pub async fn post(
    peer: IpAddr,
    port: u16,
    client: Arc<ClientConfig>,
    path: &str,
    body: &[u8],
) -> anyhow::Result<()> {
    let tcp = TcpStream::connect((peer, port)).await?;
    let connector = TlsConnector::from(client);
    let server_name = ServerName::IpAddress(peer.into());
    let mut tls = connector.connect(server_name, tcp).await?;

    let host = peer.to_string();
    let header = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    tls.write_all(header.as_bytes()).await?;
    tls.write_all(body).await?;
    tls.flush().await?;

    // Drain the response (ignore status; gossip is best-effort).
    let mut buf = vec![0u8; 1024];
    loop {
        match tls.read(&mut buf).await {
            Ok(0) => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    Ok(())
}
