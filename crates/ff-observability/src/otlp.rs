//! OTLP exporter — emits OpenTelemetry spans to a remote OTLP endpoint.
//!
//! Used by forgefleetd to ship per-session/per-tool spans to Langfuse
//! (LANG.1, self-hosted at `http://taylor:53000`). Spans carry FF-flavored
//! resource attributes (computer, tier, fabric) so Langfuse views can
//! filter by fleet identity.
//!
//! Toggle on via env var `FORGEFLEET_OTEL_ENDPOINT=http://taylor:53000/api/public/otel`.
//! When unset, no exporter is configured and the daemon runs with stdout/file
//! logs only (the LANG.2 default — opt-in tracing).

use opentelemetry::KeyValue;
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace as sdktrace;
use tracing::Subscriber;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::registry::LookupSpan;

/// Build an OpenTelemetry `OpenTelemetryLayer` configured to export to an
/// OTLP HTTP endpoint. Sets the global tracer provider as a side effect so
/// downstream `opentelemetry::global::tracer()` calls hit the same exporter.
///
/// Returns `Ok(None)` when `endpoint` is empty — caller treats that as
/// "OTLP disabled, run the daemon without remote tracing."
///
/// `service_name` becomes the OTEL `service.name` resource attribute.
/// `worker_name` becomes the FF-flavored `ff.computer` attribute so every
/// span emitted by this daemon is filterable by fleet identity in Langfuse.
/// `extra_attrs` is a key/value list merged into the resource — typical
/// callers add `ff.role`, `ff.fabric`, `ff.tier`, etc.
pub fn build_otlp_layer<S>(
    endpoint: &str,
    service_name: &str,
    worker_name: Option<&str>,
    extra_attrs: &[(String, String)],
) -> anyhow::Result<Option<OpenTelemetryLayer<S, sdktrace::Tracer>>>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    if endpoint.is_empty() {
        return Ok(None);
    }

    // Optional Basic auth header — Langfuse requires it on /api/public/otel/*.
    // Caller sets FORGEFLEET_OTEL_AUTH=<user:pass> (Langfuse expects
    // <public_key:secret_key>). We base64-encode + format as Basic.
    let mut headers = std::collections::HashMap::new();
    if let Ok(auth) = std::env::var("FORGEFLEET_OTEL_AUTH")
        && !auth.is_empty()
    {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(auth.as_bytes());
        headers.insert("Authorization".to_string(), format!("Basic {encoded}"));
    }

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
        .with_headers(headers)
        .build()?;

    let mut resource_attrs = vec![KeyValue::new("service.name", service_name.to_string())];
    if let Some(node) = worker_name {
        resource_attrs.push(KeyValue::new("ff.computer", node.to_string()));
    }
    for (k, v) in extra_attrs {
        resource_attrs.push(KeyValue::new(k.clone(), v.clone()));
    }

    let resource = Resource::new(resource_attrs);

    // We use `with_simple_exporter` (sync, in-line) rather than
    // `with_batch_exporter(_, Tokio)` because the batch+Tokio combo
    // panics at daemon shutdown ("Cannot drop a runtime in a context
    // where blocking is not allowed") — the batch processor spawns
    // a tokio task whose drop runs blocking shutdown inside the
    // outer runtime's drop. Sync export is fine at our scale
    // (10²-10³ spans/sec for the daemon).
    let provider = sdktrace::TracerProvider::builder()
        .with_simple_exporter(exporter)
        .with_resource(resource)
        .build();

    let tracer = {
        use opentelemetry::trace::TracerProvider as _;
        provider.tracer(service_name.to_string())
    };

    // Install as global so opentelemetry::global::tracer() works for any
    // crate that wants to emit spans without going through tracing macros.
    opentelemetry::global::set_tracer_provider(provider);

    Ok(Some(tracing_opentelemetry::layer().with_tracer(tracer)))
}
