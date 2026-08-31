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
///
/// The console layer is always installed at `cfg.log_level` (default info) so
/// the daemon is never silent, even with OTEL disabled — this doubles as the
/// `log_level` knob used to debug tests. OTLP layers are added only for the
/// signals that are enabled AND have a resolved endpoint.
pub fn install(
    cfg: &OTelConfig,
    service_name: &str,
    service_version: &str,
) -> anyhow::Result<TelemetryGuard> {
    let global_on = cfg.enabled.unwrap_or(true);
    if !global_on {
        info!("OTEL disabled (enabled=false)");
    }

    // Per-signal effective endpoints. A signal is on only when it is enabled
    // AND its resolved endpoint (signal override, else global) is non-empty.
    let trace_ep = sig_endpoint(&cfg.traces, &cfg.endpoint, "/v1/traces")?;
    let metric_ep = sig_endpoint(&cfg.metrics, &cfg.endpoint, "/v1/metrics")?;
    let log_ep = sig_endpoint(&cfg.logs, &cfg.endpoint, "/v1/logs")?;

    let traces_on = global_on && cfg.traces.is_enabled(true) && trace_ep.is_some();
    let metrics_on = global_on && cfg.metrics.is_enabled(true) && metric_ep.is_some();
    let logs_on = global_on && cfg.logs.is_enabled(true) && log_ep.is_some();

    if global_on && !(traces_on || metrics_on || logs_on) {
        info!("OTEL enabled but no signal endpoint configured");
    }

    // Effective service identity: config wins, caller-provided is the fallback.
    let service_name = cfg
        .service_name
        .clone()
        .unwrap_or_else(|| service_name.to_string());

    let resource = Resource::builder()
        .with_service_name(service_name.clone())
        .with_attribute(opentelemetry::KeyValue::new(
            "service.version",
            service_version.to_string(),
        ))
        .build();

    // MeterProvider first, then LoggerProvider, then the tracing bridge and
    // TracerProvider last (so OTel logs/meters interpolate correctly while the
    // global subscriber starts assimilating events). See opentelemetry-rust
    // docs/design/observability.md for the recommended ordering.
    let mp = if metrics_on {
        let exp = opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .with_endpoint(metric_ep.unwrap())
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
        let exp = opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .with_endpoint(log_ep.unwrap())
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
        let exp = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .with_endpoint(trace_ep.unwrap())
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

    // Console filter follows the global log_level; the OTLP layers follow the
    // per-signal level, which defaults to the global one but may be lower
    // (e.g. info on the console, debug forwarded to the collector).
    let console_filter = env_filter(cfg.log_level.as_deref().unwrap_or("info"));
    let otel_filter = env_filter(
        cfg.level
            .as_deref()
            .unwrap_or(cfg.log_level.as_deref().unwrap_or("info")),
    );

    let registry = tracing_subscriber::registry();
    let registry = registry.with(tracing_subscriber::fmt::layer().with_filter(console_filter));

    let lp_layer = match lp.as_ref() {
        Some(lp) => opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(lp)
            .with_filter(otel_filter.clone())
            .boxed(),
        None => tracing_subscriber::fmt::layer()
            .with_filter(tracing::level_filters::LevelFilter::OFF)
            .boxed(),
    };
    let registry = registry.with(lp_layer);

    let tp_layer = match tp.as_ref() {
        Some(tp) => tracing_opentelemetry::layer()
            .with_tracer(tp.tracer(service_name.to_string()))
            .with_filter(otel_filter)
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
        "otel={} endpoint={} traces={} metrics={} logs={}",
        global_on, cfg.endpoint, traces_on, metrics_on, logs_on
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

/// Resolve and validate a signal's effective endpoint (signal override, else
/// global). None when the resolved endpoint is empty (signal not configured);
/// validates the http/https scheme so a bad scheme fails loudly at startup.
fn sig_endpoint(
    sig: &OTelSignalConfig,
    global: &str,
    path: &str,
) -> anyhow::Result<Option<String>> {
    let ep = sig.effective_endpoint(global);
    if ep.is_empty() {
        return Ok(None);
    }
    config::parse_endpoint(&ep).map_err(anyhow::Error::from)?;
    Ok(Some(signal_endpoint(&ep, path)))
}

fn env_filter(level: &str) -> EnvFilter {
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
