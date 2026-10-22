//! QUIC carrier (quinn over rustls/ring). mTLS comes from the rustls configs
//! the caller builds: the same CA pins every peer, the server demands a
//! client certificate, the client verifies the server name it dialled.

use crate::session::{BiStream, BoxFuture, Session};
use crate::ALPN;
use anyhow::Context;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;

/// Keepalive holds NAT bindings and detects a dead peer within `IDLE`.
const KEEP_ALIVE: Duration = Duration::from_secs(15);
const IDLE: Duration = Duration::from_secs(60);

fn transport() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(KEEP_ALIVE));
    t.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(IDLE).expect("idle timeout fits a VarInt"),
    ));
    Arc::new(t)
}

/// A bound QUIC endpoint accepting sessions.
pub struct Listener {
    ep: quinn::Endpoint,
}

impl Listener {
    /// Bind `addr` with a rustls server config (client-cert verifier set).
    pub fn bind(tls: Arc<rustls::ServerConfig>, addr: SocketAddr) -> anyhow::Result<Self> {
        let mut tls = (*tls).clone();
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
            .context("QUIC server TLS config (TLS 1.3 required)")?;
        let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        cfg.transport_config(transport());
        let ep = quinn::Endpoint::server(cfg, addr).with_context(|| format!("bind {addr}"))?;
        Ok(Listener { ep })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.ep.local_addr()
    }

    /// Next handshaken session; `None` once the endpoint is closed. A failed
    /// handshake (bad certificate, wrong ALPN) is reported as `Some(Err)`.
    pub async fn accept(&self) -> Option<anyhow::Result<QuicSession>> {
        let incoming = self.ep.accept().await?;
        let remote = incoming.remote_address();
        Some(
            incoming
                .await
                .map(QuicSession)
                .with_context(|| format!("handshake with {remote}")),
        )
    }

    pub fn close(&self) {
        self.ep.close(0u32.into(), b"shutdown");
    }
}

/// Dials sessions with a rustls client config (CA roots + own identity).
/// One UDP socket per address family, created on first use.
pub struct Dialer {
    cfg: quinn::ClientConfig,
    v4: OnceCell<quinn::Endpoint>,
    v6: OnceCell<quinn::Endpoint>,
}

impl Dialer {
    pub fn new(tls: Arc<rustls::ClientConfig>) -> anyhow::Result<Self> {
        let mut tls = (*tls).clone();
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
            .context("QUIC client TLS config (TLS 1.3 required)")?;
        let mut cfg = quinn::ClientConfig::new(Arc::new(crypto));
        cfg.transport_config(transport());
        Ok(Dialer {
            cfg,
            v4: OnceCell::new(),
            v6: OnceCell::new(),
        })
    }

    async fn endpoint(&self, addr: &SocketAddr) -> anyhow::Result<&quinn::Endpoint> {
        let (cell, bind) = match addr.ip() {
            IpAddr::V4(_) => (&self.v4, SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0)),
            IpAddr::V6(_) => (&self.v6, SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0)),
        };
        cell.get_or_try_init(|| async {
            quinn::Endpoint::client(bind).with_context(|| format!("bind {bind}"))
        })
        .await
    }

    /// Dial `addr`, verifying the certificate against `server_name` (a DNS
    /// name or the literal address), within `timeout`.
    pub async fn dial(
        &self,
        addr: SocketAddr,
        server_name: &str,
        timeout: Duration,
    ) -> anyhow::Result<QuicSession> {
        let ep = self.endpoint(&addr).await?;
        let connecting = ep
            .connect_with(self.cfg.clone(), addr, server_name)
            .with_context(|| format!("dial {addr}"))?;
        let conn = tokio::time::timeout(timeout, connecting)
            .await
            .map_err(|_| anyhow::anyhow!("dial {addr}: timed out after {timeout:?}"))?
            .with_context(|| format!("dial {addr}"))?;
        Ok(QuicSession(conn))
    }
}

/// A QUIC connection as a [`Session`].
#[derive(Clone)]
pub struct QuicSession(pub quinn::Connection);

impl QuicSession {
    /// DER certificates the peer presented, leaf first (mTLS: always some).
    pub fn peer_certs(&self) -> Vec<rustls::pki_types::CertificateDer<'static>> {
        self.0
            .peer_identity()
            .and_then(|id| {
                id.downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
                    .ok()
            })
            .map(|v| *v)
            .unwrap_or_default()
    }
}

fn conn_err(e: quinn::ConnectionError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::ConnectionAborted, e.to_string())
}

impl Session for QuicSession {
    fn open(&self) -> BoxFuture<'_, std::io::Result<BiStream>> {
        Box::pin(async move {
            let (send, recv) = self.0.open_bi().await.map_err(conn_err)?;
            Ok(BiStream {
                send: Box::new(send),
                recv: Box::new(recv),
            })
        })
    }

    fn accept(&self) -> BoxFuture<'_, std::io::Result<BiStream>> {
        Box::pin(async move {
            let (send, recv) = self.0.accept_bi().await.map_err(conn_err)?;
            Ok(BiStream {
                send: Box::new(send),
                recv: Box::new(recv),
            })
        })
    }

    fn closed(&self) -> BoxFuture<'_, String> {
        Box::pin(async move { self.0.closed().await.to_string() })
    }

    fn close(&self, reason: &str) {
        self.0.close(0u32.into(), reason.as_bytes());
    }

    fn remote(&self) -> SocketAddr {
        self.0.remote_address()
    }
}

#[cfg(any(test, feature = "testpki"))]
pub mod testpki {
    //! An in-memory CA with a server and a client leaf, for carrier tests.
    use rcgen::{
        BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    };
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use rustls::{ClientConfig, RootCertStore, ServerConfig};
    use std::sync::Arc;

    pub struct Pki {
        pub server: Arc<ServerConfig>,
        pub client: Arc<ClientConfig>,
    }

    fn leaf(
        issuer: &Issuer<'_, KeyPair>,
        sans: Vec<String>,
        eku: ExtendedKeyUsagePurpose,
    ) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        let key = KeyPair::generate().unwrap();
        let mut p = CertificateParams::new(sans).unwrap();
        p.extended_key_usages = vec![eku];
        let cert = p.signed_by(&key, issuer).unwrap();
        (
            vec![cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
    }

    pub fn pki() -> Pki {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let issuer = Issuer::new(ca_params, ca_key);

        let mut roots = RootCertStore::empty();
        roots.add(ca_cert.der().clone()).unwrap();
        let roots = Arc::new(roots);

        let (s_chain, s_key) = leaf(
            &issuer,
            vec!["127.0.0.1".into(), "localhost".into()],
            ExtendedKeyUsagePurpose::ServerAuth,
        );
        let (c_chain, c_key) = leaf(
            &issuer,
            vec!["node-0".into()],
            ExtendedKeyUsagePurpose::ClientAuth,
        );

        let verifier = rustls::server::WebPkiClientVerifier::builder(roots.clone())
            .build()
            .unwrap();
        let server = ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(s_chain, s_key)
            .unwrap();
        let client = ClientConfig::builder()
            .with_root_certificates((*roots).clone())
            .with_client_auth_cert(c_chain, c_key)
            .unwrap();
        Pki {
            server: Arc::new(server),
            client: Arc::new(client),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testpki::pki;
    use super::*;
    use crate::session::{request, respond};
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Ping {
        n: u32,
    }

    async fn pair() -> (Listener, QuicSession, QuicSession) {
        let pki = pki();
        let listener = Listener::bind(pki.server, "127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        let dialer = Dialer::new(pki.client).unwrap();
        let (client, server) = tokio::join!(
            dialer.dial(addr, "127.0.0.1", Duration::from_secs(5)),
            listener.accept()
        );
        (listener, client.unwrap(), server.unwrap().unwrap())
    }

    #[tokio::test]
    async fn streams_flow_both_ways_over_mtls() {
        let (_l, client, server) = pair().await;
        assert!(!server.peer_certs().is_empty(), "client cert presented");

        // Client-opened request, server answers on the same stream.
        let echo = tokio::spawn({
            let server = server.clone();
            async move {
                let s = server.accept().await.unwrap();
                respond(s, |p: Ping| async move { Some(Ping { n: p.n + 1 }) })
                    .await
                    .unwrap();
            }
        });
        let r: Ping = request(&client, &Ping { n: 1 }, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(r, Ping { n: 2 });
        echo.await.unwrap();

        // Server-opened request: the thing HTTP could not do through NAT.
        let answer = tokio::spawn({
            let client = client.clone();
            async move {
                let s = client.accept().await.unwrap();
                respond(s, |p: Ping| async move { Some(Ping { n: p.n * 10 }) })
                    .await
                    .unwrap();
            }
        });
        let r: Ping = request(&server, &Ping { n: 4 }, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(r, Ping { n: 40 });
        answer.await.unwrap();

        // Close propagates: accept fails and `closed` resolves on the peer.
        client.close("bye");
        assert!(server.accept().await.is_err());
        let reason = server.closed().await;
        assert!(reason.contains("bye"), "{reason}");
    }

    #[tokio::test]
    async fn request_times_out_when_unanswered() {
        let (_l, client, server) = pair().await;
        let hold = tokio::spawn({
            let server = server.clone();
            async move {
                let s = server.accept().await.unwrap();
                tokio::time::sleep(Duration::from_secs(5)).await;
                drop(s);
            }
        });
        let err = request::<_, Ping>(&client, &Ping { n: 1 }, Duration::from_millis(200))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        hold.abort();
    }

    #[tokio::test]
    async fn wrong_ca_is_refused() {
        let good = pki();
        let other = pki();
        let listener = Listener::bind(good.server, "127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        let dialer = Dialer::new(other.client).unwrap();
        let (client, server) = tokio::join!(
            dialer.dial(addr, "127.0.0.1", Duration::from_secs(5)),
            listener.accept()
        );
        assert!(client.is_err());
        assert!(server.unwrap().is_err());
    }
}
