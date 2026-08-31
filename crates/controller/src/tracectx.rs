//! Distributed trace correlation across the controller → agent boundary.
//!
//! The controller stamps every dispatched command with the W3C `traceparent`
//! of its dispatch span; the (uninstrumented, ucode) agent echoes that string
//! back on the reply, and the controller re-parents the ingest span under the
//! dispatch span. The result is a single trace spanning
//! `dispatch → (agent gap) → result`, even though nothing runs inside the
//! agent, and the wall-clock delta between the dispatch and reply spans is the
//! measurable round-trip.

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
