//! mTLS JSON client for the controller's clients listener.
//!
//! rustls with the ring provider (the workspace's provider, see
//! CONTRIBUTING.md), pinned to the mesh CA, authenticating with the node's
//! own certificate. Connections are kept alive between requests.

use anyhow::{anyhow, bail, Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request, Uri};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ClientConfig, RootCertStore};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io::BufReader;
use std::sync::Arc;
use std::time::Duration;

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
pub fn tls_config(cacert: &str, cert: &str, key: &str) -> Result<ClientConfig> {
    let mut roots = RootCertStore::empty();
    let (_, bad) = roots.add_parsable_certificates(load_certs(cacert)?);
    if bad > 0 {
        bail!("{bad} unparsable CA certificate(s) in {cacert}");
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS protocol versions")?
        .with_root_certificates(roots)
        .with_client_auth_cert(load_certs(cert)?, load_key(key)?)
        .context("client identity")
}

pub struct Controller {
    base: String,
    http: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
}

impl Controller {
    pub fn new(base_url: &str, tls: ClientConfig) -> Result<Controller> {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_only()
            .enable_http1()
            .build();
        let http = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(90))
            .build(https);
        base_url
            .parse::<Uri>()
            .with_context(|| format!("controller_url {base_url}"))?;
        Ok(Controller {
            base: base_url.trim_end_matches('/').to_string(),
            http,
        })
    }

    /// POST `body` as JSON to `path`, decode the JSON answer. Fails on a
    /// non-2xx status or when the whole exchange exceeds `timeout`.
    pub async fn post<B: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        timeout: Duration,
    ) -> Result<R> {
        let uri: Uri = format!("{}{}", self.base, path).parse()?;
        let req = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(serde_json::to_vec(body)?)))?;

        let exchange = async {
            let resp = self.http.request(req).await?;
            let status = resp.status();
            let bytes = resp.into_body().collect().await?.to_bytes();
            anyhow::Ok((status, bytes))
        };
        let (status, bytes) = tokio::time::timeout(timeout, exchange)
            .await
            .map_err(|_| anyhow!("POST {path}: timed out"))?
            .with_context(|| format!("POST {path}"))?;
        if !status.is_success() {
            bail!("POST {path}: HTTP {status}");
        }
        serde_json::from_slice(&bytes).with_context(|| format!("POST {path}: decode reply"))
    }
}
