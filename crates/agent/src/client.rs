//! Minimal mTLS HTTP client for talking to the controller's clients listener.
//! Built on tokio-rustls (ring), same provider as the controller.

use crate::config::TlsConfig;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use std::io::BufReader;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

pub struct Client {
    inner: Arc<ClientConfig>,
    addr: std::net::SocketAddr,
    host: String,
}

impl Client {
    pub fn new(host: &str, port: u16, tls: &TlsConfig) -> anyhow::Result<Client> {
        let addr = (host, port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| anyhow::anyhow!("resolve {host}:{port}"))?;

        let mut roots = RootCertStore::empty();
        let ca = std::fs::read_to_string(&tls.ca)?;
        let mut reader = BufReader::new(ca.as_bytes());
        let certs: Vec<_> = rustls_pemfile::certs(&mut reader).collect::<Result<_, _>>()?;
        let (_, invalid) = roots.add_parsable_certificates(certs);
        if invalid > 0 {
            anyhow::bail!("invalid CA certs in {}", tls.ca);
        }

        let cert_text = std::fs::read_to_string(&tls.cert)?;
        let key_text = std::fs::read_to_string(&tls.key)?;
        let mut cert_rd = BufReader::new(cert_text.as_bytes());
        let certs = rustls_pemfile::certs(&mut cert_rd).collect::<Result<Vec<_>, _>>()?;
        let mut key_rd = BufReader::new(key_text.as_bytes());
        let key = rustls_pemfile::private_key(&mut key_rd)?
            .ok_or_else(|| anyhow::anyhow!("no private key in {}", tls.key))?;

        let config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_client_auth_cert(certs, key)?;

        Ok(Client {
            inner: Arc::new(config),
            addr,
            host: host.to_string(),
        })
    }

    /// POST `path` with a JSON body; returns the response body as a string.
    pub async fn post_json(&self, path: &str, body: &[u8]) -> anyhow::Result<String> {
        let tcp = TcpStream::connect(self.addr).await?;
        let connector = TlsConnector::from(self.inner.clone());
        let host = self.host.clone();
        let server_name = ServerName::try_from(host)
            .unwrap_or_else(|_| ServerName::IpAddress(self.addr.ip().into()));
        let mut tls = connector.connect(server_name, tcp).await?;

        let header = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.host,
            body.len()
        );
        tls.write_all(header.as_bytes()).await?;
        tls.write_all(body).await?;
        tls.shutdown().await?;

        let mut response = Vec::new();
        let mut buf = [0u8; 2048];
        loop {
            match tls.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => response.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        let text = String::from_utf8_lossy(&response);
        // Strip the HTTP status line and headers, keep the body.
        Ok(split_body(&text).to_string())
    }
}

fn split_body(raw: &str) -> &str {
    match raw.find("\r\n\r\n") {
        Some(i) => raw[i + 4..].trim(),
        None => raw.trim(),
    }
}
