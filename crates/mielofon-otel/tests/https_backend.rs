//! The OTLP exporter's HTTP client must be able to speak `https://`: the
//! collector endpoint in production is TLS. opentelemetry-otlp pulls reqwest
//! without a TLS backend, so this guards the feature set of our own reqwest
//! dependency. Nothing listens on the port: the failure must be the
//! connection, never the URL scheme.

#[test]
fn https_scheme_is_supported_by_the_exporter_client() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .expect("client builds with a TLS backend");
    let err = client
        .post("https://127.0.0.1:1/v1/traces")
        .body(Vec::<u8>::new())
        .send()
        .expect_err("nothing listens there");
    assert!(
        err.is_connect() || err.is_timeout(),
        "expected a connection failure, got: {err:?}"
    );
}
