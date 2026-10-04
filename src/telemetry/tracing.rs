//! Distributed tracing: the Datadog tracer provider and the `tracing` bridge.
//!
//! # Declaring spans
//!
//! Sync code creates spans with `tracing` macros and the bridge layer turns
//! every span field into an OpenTelemetry attribute (Datadog tag). Fields known
//! only at the end are declared `tracing::field::Empty` and recorded later:
//!
//! ```ignore
//! let span = tracing::info_span!(
//!     "ferry.sync_repo",
//!     repo = %entry.repo_tag(),
//!     forgejo_repo = %forgejo,
//!     result = tracing::field::Empty,
//!     error_kind = tracing::field::Empty,
//!     refs_changed = tracing::field::Empty,
//! );
//! let git = tracing::info_span!(
//!     "git.fetch",
//!     git.side = "github",
//!     git.exit_code = tracing::field::Empty,
//! );
//! let api = tracing::info_span!(
//!     "forgejo.api",
//!     http.method = "GET",
//!     http.route = "/api/v1/repos/{owner}/{repo}",   // template, never the URL
//!     http.status_code = tracing::field::Empty,
//! );
//! ```
//!
//! Rules:
//!
//! * Use `info_span!` (or higher). The default level filter is `info`, and a
//!   span filtered out by level is neither exported nor a parent.
//! * A field must be declared in the macro to be recordable later. Recording
//!   an undeclared field is silently dropped.
//! * Never put a URL with a query string, a header, a token, or git stderr in
//!   a field (see the secret rules in `AGENTS.md`).
//! * Mark an error outcome with `mark_error(&span, kind.as_str())`.
//!
//! # Span name, operation name, resource name
//!
//! `datadog-opentelemetry` does not use the OpenTelemetry span name as the
//! Datadog operation name. It uses the `operation.name` attribute when
//! present, and otherwise derives a name from the span kind and semantic
//! convention attributes; a plain internal span becomes `internal`. Without
//! help, a span named `ferry.sync_repo` would NOT match
//! `operation_name:ferry.sync_repo`. `OperationNameProcessor` (installed on the
//! provider built here) copies the span name into `operation.name` at span
//! start unless the span already set it, so the `tracing` span name is the
//! Datadog operation name. The resource name falls back to the span name too.
//!
//! The processor lives on the provider, not on the layer. A test provider
//! built elsewhere must add `OperationNameProcessor` itself if it wants to
//! assert on `operation.name`.

use std::time::Duration;

use ::tracing::Span;
use datadog_opentelemetry::configuration::Config;
use opentelemetry::trace::{Status, TracerProvider as _};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider, SpanData, SpanProcessor};
use tracing_opentelemetry::{OpenTelemetryLayer, OpenTelemetrySpanExt};
use tracing_subscriber::registry::LookupSpan;

use super::ConstTags;

/// Instrumentation scope name of the tracer.
pub const TRACER_NAME: &str = "ferry";

/// Attribute that `datadog-opentelemetry` reads as the Datadog operation name.
pub const OPERATION_NAME_ATTRIBUTE: &str = "operation.name";

/// Default time `shutdown` waits for the final flush.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Validates a `DD_TRACE_AGENT_URL` value: `http(s)://host[:port]` or
/// `unix:///path/to/apm.socket`. Returns the trimmed URL.
pub fn parse_agent_url(value: &str) -> Result<String, String> {
    let value = value.trim();
    let url = url::Url::parse(value).map_err(|e| format!("not a valid URL: {e}"))?;
    match url.scheme() {
        "http" | "https" => {
            if url.host_str().is_none_or(str::is_empty) {
                return Err("URL needs a host".to_owned());
            }
        }
        "unix" => {
            if url.path().is_empty() || url.path() == "/" {
                return Err("unix URL needs a socket path (unix:///path)".to_owned());
            }
        }
        other => {
            return Err(format!(
                "unsupported scheme `{other}` (use http://host:8126 or unix:///path)"
            ));
        }
    }
    Ok(value.to_owned())
}

/// Copies the span name into `operation.name` so that Datadog's operation name
/// equals the `tracing` span name. See the module docs.
#[derive(Debug, Default, Clone, Copy)]
pub struct OperationNameProcessor;

impl SpanProcessor for OperationNameProcessor {
    fn on_start(&self, span: &mut opentelemetry_sdk::trace::Span, _cx: &Context) {
        use opentelemetry::trace::Span as _;
        let Some(data) = span.exported_data() else {
            return;
        };
        if data
            .attributes
            .iter()
            .any(|kv| kv.key.as_str() == OPERATION_NAME_ATTRIBUTE)
        {
            return;
        }
        span.set_attribute(KeyValue::new(OPERATION_NAME_ATTRIBUTE, data.name));
    }

    fn on_end(&self, _span: SpanData) {}

    fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> opentelemetry_sdk::error::OTelSdkResult {
        Ok(())
    }
}

/// Builds the Datadog tracer provider for `agent_url` (already validated with
/// `parse_agent_url`).
///
/// All traces are exported: the sample rate is pinned to 1.0 and remote
/// configuration is off, so the Agent cannot push sampling rules. Datadog's
/// product telemetry is off too. The provider is local: it is not installed as
/// the OpenTelemetry global. It owns its own threads and needs no tokio
/// runtime, so it can be built and shut down from any thread.
pub fn build_datadog_provider(tags: &ConstTags, agent_url: &str) -> SdkTracerProvider {
    let mut builder = Config::builder();
    builder
        .set_service(tags.service.clone())
        .set_version(tags.version.clone())
        .set_trace_agent_url(agent_url.to_owned())
        .set_trace_sample_rate(1.0)
        .set_remote_config_enabled(false)
        .set_telemetry_enabled(false);
    if let Some(env) = &tags.env {
        builder.set_env(env.clone());
    }
    let config = builder.build();
    let (provider, _propagator) = datadog_opentelemetry::tracing()
        .with_config(config)
        .with_span_processor(OperationNameProcessor)
        .init_local();
    provider
}

/// The `tracing` layer that exports spans through `provider`. Works with any
/// `opentelemetry_sdk` provider, including one with an in-memory exporter.
pub fn otel_layer<S>(provider: &SdkTracerProvider) -> OpenTelemetryLayer<S, SdkTracer>
where
    S: ::tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    tracing_opentelemetry::layer().with_tracer(provider.tracer(TRACER_NAME))
}

/// Makes the exported span an error span with `error.type = error_kind`.
///
/// `error_kind` must be an `ErrorKind::as_str` value, never free text from a
/// remote system.
pub fn mark_error(span: &Span, error_kind: &str) {
    span.set_attribute("error.type", error_kind.to_owned());
    span.set_status(Status::error(error_kind.to_owned()));
}
