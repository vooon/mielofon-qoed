//! Distributed trace correlation across the controller ↔ agent boundary.
//!
//! The controller stamps every dispatched command with the W3C `traceparent`
//! of its dispatch span. The agent opens its command span under that parent
//! (so its iperf3 legs and BIRD calls land in the same trace) and echoes the
//! string back on the reply, under which the controller re-parents its
//! ingest span. One trace spans `dispatch → agent.command → report`.

use opentelemetry::propagation::{Extractor, Injector, TextMapPropagator};
use opentelemetry::trace::TraceContextExt;
use opentelemetry::Context;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Inject the current tracing span's trace context as a W3C `traceparent`.
/// Returns `None` when no OpenTelemetry parent is present (OTEL disabled).
pub fn current_traceparent() -> Option<String> {
    let cx = tracing::Span::current().context();
    if !cx.span().span_context().is_valid() {
        return None;
    }
    let mut carrier = TraceCarrier::default();
    TraceContextPropagator::new().inject_context(&cx, &mut carrier);
    carrier.0.get("traceparent").cloned()
}

/// Parse a W3C `traceparent` (possibly `None`) back into a context usable as a
/// parent for subsequent spans. Returns `None` if unparseable/invalid.
pub fn parent_context(traceparent: Option<&str>) -> Option<Context> {
    let tp = traceparent?;
    let mut carrier = TraceCarrier::default();
    carrier.0.insert("traceparent".to_string(), tp.to_string());
    let cx = TraceContextPropagator::new().extract(&carrier);
    cx.span().span_context().is_valid().then_some(cx)
}

/// Parent `span` under `traceparent` when it carries a valid context; a
/// missing or malformed one leaves the span a root (OTEL off on the sender).
pub fn set_parent(span: &tracing::Span, traceparent: Option<&str>) {
    if let Some(cx) = parent_context(traceparent) {
        let _ = span.set_parent(cx);
    }
}

/// A minimal injector/extractor carrier (single `traceparent` entry).
#[derive(Default)]
struct TraceCarrier(std::collections::HashMap<String, String>);

impl Injector for TraceCarrier {
    fn set(&mut self, key: &str, value: String) {
        self.0.insert(key.to_string(), value);
    }
}

impl Extractor for TraceCarrier {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_extract_roundtrip() {
        let props = [
            "00",
            "4bf92f3577b34da6a3ce929d0e0e4736",
            "00f067aa0ba902b7",
            "01",
        ];
        let tp = props.join("-");
        let cx = parent_context(Some(&tp)).expect("valid parent");
        assert!(cx.span().span_context().is_valid());
        assert_eq!(
            cx.span().span_context().trace_id().to_string(),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
        // Invalid (all-zero) ids → None
        let bad = "00-00000000000000000000000000000000-0000000000000000-01";
        assert!(parent_context(Some(bad)).is_none());
    }
}
