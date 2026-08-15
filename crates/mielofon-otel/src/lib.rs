//! Shared OpenTelemetry (OTLP/HTTP, gRPC-free) setup for mielofon daemons.
//!
//! Modeled on `pathosd/internal/telemetry`: a single collector endpoint, three
//! independently-enablable signals (traces, metrics, logs), optional per-signal
//! overrides. Transport is HTTP(S) only — the `grpc-tonic` feature of
//! `opentelemetry-otlp` is deliberately not enabled, so no gRPC is pulled in.
//! An empty endpoint or disabled config yields no-op providers.

mod config;

pub use config::{OTelConfig, OTelSignalConfig, ParseError};

use anyhow::Context;
use tracing::{debug, info, level_filters::LevelFilter};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

use opentelemetry::global;
use opentelemetry::trace::TracerProvider;
use opentelemetry_sdk::resource::Resource;

/// RAII guard holding OTEL providers; flushes and shuts them down once.
pub struct TelemetryGuard {
    tp: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
    lp: Option<opentelemetry_sdk::logs::SdkLoggerProvider>,
}

impl TelemetryGuard {
    /// Flush all providers. Idempotent.
    pub fn shutdown(&self) {
        if let Some(lp) = &self.lp {
            let _ = lp.shutdown();
        }
        if let Some(tp) = &self.tp {
            let _ = tp.shutdown();
        }
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Build the OTEL providers, install a global `tracing` subscriber (console
/// layer + optional OTLP log bridge + OTLP trace layer), and set the global
/// meter provider. Returns a guard to keep alive for the daemon's lifetime.
pub fn install(
    cfg: &OTelConfig,
    service_name: &str,
    service_version: &str,
) -> anyhow::Result<TelemetryGuard> {
    if !cfg.is_enabled() {
        info!("OTEL disabled (endpoint empty or enabled=false)");
        return Ok(TelemetryGuard { tp: None, lp: None });
    }

    config::parse_endpoint(&cfg.endpoint).map_err(anyhow::Error::from)?;

    let resource = Resource::builder()
        .with_service_name(service_name.to_string())
        .with_attribute(opentelemetry::KeyValue::new(
            "service.version",
            service_version.to_string(),
        ))
        .build();

    let traces_on = cfg.traces.is_enabled(true);
    let metrics_on = cfg.metrics.is_enabled(true);
    let logs_on = cfg.logs.is_enabled(true);

    if !(traces_on || metrics_on || logs_on) {
        info!("OTEL endpoint set but all signals disabled");
        return Ok(TelemetryGuard { tp: None, lp: None });
    }

    // MeterProvider first, then LoggerProvider, then the tracing bridge and
    // TracerProvider last (so OTel logs/meters interpolate correctly while the
    // global subscriber starts assimilating events). See opentelemetry-rust
    // docs/design/observability.md for the recommended ordering.
    let mp = if metrics_on {
        let exporter = signal_endpoint(&cfg.endpoint, "/v1/metrics");
        let exp = opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .with_endpoint(exporter)
            .build()
            .context("build OTEL metric exporter")?;
        Some(
            opentelemetry_sdk::metrics::SdkMeterProvider::builder()
                .with_periodic_exporter(exp)
                .with_resource(resource.clone())
                .build(),
        )
    } else {
        None
    };

    let lp = if logs_on {
        let exporter = signal_endpoint(&cfg.endpoint, "/v1/logs");
        let exp = opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .with_endpoint(exporter)
            .build()
            .context("build OTEL log exporter")?;
        Some(
            opentelemetry_sdk::logs::SdkLoggerProvider::builder()
                .with_batch_exporter(exp)
                .with_resource(resource.clone())
                .build(),
        )
    } else {
        None
    };

    let tp = if traces_on {
        let exporter = signal_endpoint(&cfg.endpoint, "/v1/traces");
        let exp = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .with_endpoint(exporter)
            .build()
            .context("build OTEL trace exporter")?;
        Some(
            opentelemetry_sdk::trace::SdkTracerProvider::builder()
                .with_batch_exporter(exp)
                .with_resource(resource)
                .build(),
        )
    } else {
        None
    };

    if let Some(mp) = &mp {
        global::set_meter_provider(mp.clone());
    }

    let filter = env_filter(cfg);
    let registry = tracing_subscriber::registry();
    let registry = registry.with(tracing_subscriber::fmt::layer().with_filter(filter.clone()));

    let lp_layer = match lp.as_ref() {
        Some(lp) => opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(lp)
            .with_filter(filter.clone())
            .boxed(),
        None => tracing_subscriber::fmt::layer()
            .with_filter(tracing::level_filters::LevelFilter::OFF)
            .boxed(),
    };
    let registry = registry.with(lp_layer);

    let tp_layer = match tp.as_ref() {
        Some(tp) => tracing_opentelemetry::layer()
            .with_tracer(tp.tracer(service_name.to_string()))
            .with_filter(filter)
            .boxed(),
        None => tracing_subscriber::fmt::layer()
            .with_filter(tracing::level_filters::LevelFilter::OFF)
            .boxed(),
    };
    let registry = registry.with(tp_layer);

    registry
        .try_init()
        .context("failed to install global tracing subscriber")?;

    debug!(
        "OTEL enabled endpoint={} traces={} metrics={} logs={}",
        cfg.endpoint, traces_on, metrics_on, logs_on
    );

    Ok(TelemetryGuard { tp, lp })
}

/// Append the OTLP signal path when the configured endpoint carries no path.
/// (otlptracehttp/otlpmetrichttp normalize internally, but the http log
/// exporter does not always append `/v1/logs`; being explicit is safe for all.)
fn signal_endpoint(endpoint: &str, path: &str) -> String {
    let has_path = endpoint
        .split_once("://")
        .map(|(_, rest)| rest.contains('/'))
        .unwrap_or(true);
    if has_path {
        endpoint.trim_end_matches('/').to_string()
    } else {
        format!("{}{}", endpoint.trim_end_matches('/'), path)
    }
}

fn env_filter(cfg: &OTelConfig) -> EnvFilter {
    let level = cfg.level.as_deref().unwrap_or("info");
    EnvFilter::builder()
        .with_default_directive(LevelFilter::from(parse_level(level)).into())
        .from_env_lossy()
}

fn parse_level(level: &str) -> tracing::Level {
    match level.to_ascii_lowercase().as_str() {
        "trace" => tracing::Level::TRACE,
        "debug" => tracing::Level::DEBUG,
        "warn" => tracing::Level::WARN,
        "error" => tracing::Level::ERROR,
        _ => tracing::Level::INFO,
    }
}

use opentelemetry_otlp::WithExportConfig;
