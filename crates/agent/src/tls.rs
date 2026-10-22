//! The agent's mTLS identity for the controller session: rustls with the
//! ring provider, pinned to the mesh CA, authenticating with the node's
//! own certificate.

use anyhow::{anyhow, bail, Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ClientConfig, RootCertStore};
use std::io::BufReader;
use std::sync::Arc;

fn load_certs(path: &str) -> Result<Vec<CertificateDer<'static>>> {
    let pem = std::fs::read(path).with_context(|| format!("read {path}"))?;
    let certs: Vec<_> = rustls_pemfile::certs(&mut BufReader::new(pem.as_slice()))
        .collect::<Result<_, _>>()
        .with_context(|| format!("parse {path}"))?;
    if certs.is_empty() {
        bail!("no certificates in {path}");
    }
    Ok(certs)
}

fn load_key(path: &str) -> Result<PrivateKeyDer<'static>> {
    let pem = std::fs::read(path).with_context(|| format!("read {path}"))?;
    rustls_pemfile::private_key(&mut BufReader::new(pem.as_slice()))
        .with_context(|| format!("parse {path}"))?
        .ok_or_else(|| anyhow!("no private key in {path}"))
}

/// rustls client config: mesh CA roots + client identity.
pub fn client_config(cacert: &str, cert: &str, key: &str) -> Result<Arc<ClientConfig>> {
    let mut roots = RootCertStore::empty();
    let (_, bad) = roots.add_parsable_certificates(load_certs(cacert)?);
    if bad > 0 {
        bail!("{bad} unparsable CA certificate(s) in {cacert}");
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS protocol versions")?
        .with_root_certificates(roots)
        .with_client_auth_cert(load_certs(cert)?, load_key(key)?)
        .context("client identity")?;
    Ok(Arc::new(cfg))
}
