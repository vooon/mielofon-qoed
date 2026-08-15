//! OTEL configuration model. Mirrors `pathosd`'s `otel` config block but with
//! an HTTP(S)-only transport (no `grpc://` path).

use serde::Deserialize;

/// Transport protocol resolved from the endpoint scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Http,
}

/// Error returned when parsing an OTEL endpoint.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("unrecognised OTLP endpoint scheme in {0:?}; only http:// or https:// are supported (gRPC is not used)")]
    UnsupportedScheme(String),
}

/// Singleton-signal configuration. Each signal may override the global
/// endpoint per-transport; only `enabled` and `endpoint` are wired today.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct OTelSignalConfig {
    /// Per-signal endpoint URL (http/https). Empty = inherit global.
    #[serde(alias = "endpoint")]
    pub endpoint: Option<String>,
    /// Override the global enabled flag for this signal.
    pub enabled: Option<bool>,
}

impl OTelSignalConfig {
    /// Resolved endpoint: signal override or global.
    pub fn effective_endpoint(&self, global: &str) -> String {
        self.endpoint.as_deref().unwrap_or(global).to_string()
    }

    /// Whether this signal is on, honouring the global enabled default.
    pub fn is_enabled(&self, global_enabled: bool) -> bool {
        self.enabled.unwrap_or(global_enabled)
    }
}

/// Top-level OpenTelemetry configuration.
///
/// ```toml
/// [otel]
/// enabled = true
/// endpoint = "http://collector.example:4318"
/// service_name = "mielofon-controller"
/// level = "info"
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct OTelConfig {
    /// Master switch. Defaults to true; export off when endpoint is empty.
    pub enabled: Option<bool>,
    /// OTLP collector endpoint (http:// or https://). Empty disables OTEL.
    pub endpoint: String,
    /// Resource `service.name`. Usually not needed; caller passes it in.
    #[serde(alias = "service_name")]
    pub service_name: Option<String>,
    /// Minimum level forwarded to OTEL logs. Independent of console `level`.
    pub level: Option<String>,
    /// Per-signal overrides.
    pub traces: OTelSignalConfig,
    pub metrics: OTelSignalConfig,
    pub logs: OTelSignalConfig,
}

impl OTelConfig {
    /// Effective enabled state.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true) && !self.endpoint.is_empty()
    }
}

/// Resolve the transport from an endpoint URL. Only HTTP(S) is supported; the
/// returned string is the normalized URL passed to the OTEL exporters.
pub fn parse_endpoint(raw: &str) -> Result<Protocol, ParseError> {
    if raw.starts_with("grpc://") || raw.starts_with("grpcs://") {
        return Err(ParseError::UnsupportedScheme(raw.to_string()));
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return Ok(Protocol::Http);
    }
    Err(ParseError::UnsupportedScheme(raw.to_string()))
}
