//! mTLS via rustls. A dedicated CA pins every peer; each node holds a
//! server+client cert. All endpoints demand a valid, CA-signed client cert
//! (no anonymous access).

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use std::io::BufReader;
use std::sync::Arc;

use crate::config::Tls;

/// Load PEM server/client certificate chain from a path.
pub fn load_certs(path: &str) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let text =
        std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("read cert {}: {}", path, e))?;
    let mut reader = BufReader::new(text.as_bytes());
    let certs: Vec<_> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("parse cert {}: {}", path, e))?;
    if certs.is_empty() {
        anyhow::bail!("no X.509 certificates found in {}", path);
    }
    Ok(certs)
}

/// Load PEM private key from a path.
pub fn load_key(path: &str) -> anyhow::Result<PrivateKeyDer<'static>> {
    let text =
        std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("read key {}: {}", path, e))?;
    let mut reader = BufReader::new(text.as_bytes());
    rustls_pemfile::private_key(&mut reader)
        .map_err(|e| anyhow::anyhow!("parse key {}: {}", path, e))?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {}", path))
}

fn root_store(ca_paths: &[&str]) -> anyhow::Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    for path in ca_paths {
        let certs = load_certs(path)?;
        let (_, invalid) = roots.add_parsable_certificates(certs);
        if invalid > 0 {
            anyhow::bail!("failed to parse {invalid} CA cert(s) in {path}");
        }
    }
    Ok(roots)
}

/// Server config that requires a CA-signed client cert (mutual TLS).
pub fn server_config(tls: &Tls, ca_paths: &[&str]) -> anyhow::Result<Arc<ServerConfig>> {
    let roots = Arc::new(root_store(ca_paths)?);
    let verifier = rustls::server::WebPkiClientVerifier::builder(roots)
        .build()
        .map_err(|e| anyhow::anyhow!("build client verifier: {e:?}"))?;

    let certs = load_certs(&tls.cert)?;
    let key = load_key(&tls.key)?;

    let cfg = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("load server identity: {e:?}"))?;
    Ok(Arc::new(cfg))
}

/// Client config for agent/member connections that authenticate with the
/// node's own cert to the trusted CA.
pub fn client_config(tls: &Tls, ca_paths: &[&str]) -> anyhow::Result<Arc<ClientConfig>> {
    let roots = root_store(ca_paths)?;
    let certs = load_certs(&tls.cert)?;
    let key = load_key(&tls.key)?;

    let cfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("load client identity: {e:?}"))?;
    Ok(Arc::new(cfg))
}
