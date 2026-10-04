//! Structured logging.
//!
//! `JsonLogLayer` writes one JSON object per line. Fields:
//!
//! | Field | Value |
//! |---|---|
//! | `timestamp` | RFC 3339, UTC |
//! | `level` | `trace`, `debug`, `info`, `warn`, `error` |
//! | `message` | the event message |
//! | `target` | the event's module path |
//! | `service`, `env`, `version` | unified service tags (`env` omitted when unset) |
//! | event fields | at top level |
//! | enclosing span fields | at top level, outermost span first; inner spans and the event win on a name clash |
//! | `dd.trace_id`, `dd.span_id` | only inside a span that has an OpenTelemetry context |
//!
//! # Datadog log-trace correlation encoding
//!
//! `dd.trace_id` and `dd.span_id` are unsigned 64-bit integers written as
//! decimal strings. `dd.trace_id` is the LOW 64 bits of the 128-bit
//! OpenTelemetry trace id. `datadog-opentelemetry` sends exactly that value as
//! the Datadog trace id and puts the high 64 bits in the `_dd.p.tid` tag
//! (`mappings/transform/mod.rs`, `otel_trace_id_to_dd_id`; its generator makes
//! ids of the form `32-bit timestamp | 32 zero bits | 64 random bits`,
//! `trace_id.rs`). A 32-character hex id would not correlate with those
//! traces. The span id is the whole 64-bit span id in decimal.
//!
//! Reserved names (`timestamp`, `level`, `message`, `target`, `service`, `env`,
//! `version`, `dd.*`) always carry the values above, even if a span or event
//! declares a field of the same name.
//!
//! The layer is generic over `MakeWriter` so tests can capture output.

use std::io::Write;
use std::sync::OnceLock;

use ::tracing::field::{Field, Visit};
use ::tracing::span::{Attributes, Record};
use ::tracing::{Dispatch, Event, Id, Level, Subscriber, dispatcher::WeakDispatch};
use opentelemetry::trace::TraceContextExt as _;
use serde_json::{Map, Value};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use super::ConstTags;

/// Fields recorded on one span, stored in the span's extensions.
#[derive(Debug, Default)]
struct SpanFields(Map<String, Value>);

/// Collects `tracing` fields into a JSON map.
struct FieldVisitor<'a>(&'a mut Map<String, Value>);

impl FieldVisitor<'_> {
    fn insert(&mut self, field: &Field, value: Value) {
        self.0
            .insert(field.name().trim_start_matches("r#").to_owned(), value);
    }
}

impl Visit for FieldVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert(field, Value::String(value.to_owned()));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field, Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field, Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field, Value::Bool(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.insert(
            field,
            serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number),
        );
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.insert(field, Value::String(format!("{value:?}")));
    }
}

/// JSON-lines log layer. See the module docs for the output contract.
pub struct JsonLogLayer<W> {
    make_writer: W,
    tags: ConstTags,
    dispatch: OnceLock<WeakDispatch>,
}

impl<W> JsonLogLayer<W> {
    pub fn new(make_writer: W, tags: ConstTags) -> Self {
        Self {
            make_writer,
            tags,
            dispatch: OnceLock::new(),
        }
    }

    /// Datadog correlation ids of the span the event belongs to, if that span
    /// has a valid OpenTelemetry context.
    fn dd_ids<S>(&self, ctx: &Context<'_, S>, event: &Event<'_>) -> Option<(String, String)>
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        let span = ctx.event_span(event)?;
        let dispatch = self.dispatch.get()?.upgrade()?;
        // No extensions guard is held here: `get_otel_context` locks them.
        let otel = tracing_opentelemetry::get_otel_context(&span.id(), &dispatch)?;
        let span_ref = otel.span();
        let span_context = span_ref.span_context();
        if !span_context.is_valid() {
            return None;
        }
        Some(dd_ids(span_context.trace_id(), span_context.span_id()))
    }
}

/// The Datadog log-correlation encoding of an OpenTelemetry trace and span id:
/// `(low 64 bits of the trace id, span id)`, each as a decimal string.
pub fn dd_ids(
    trace_id: opentelemetry::TraceId,
    span_id: opentelemetry::SpanId,
) -> (String, String) {
    let bytes = trace_id.to_bytes();
    let low = u64::from_be_bytes([
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    ]);
    (
        low.to_string(),
        u64::from_be_bytes(span_id.to_bytes()).to_string(),
    )
}

fn level_str(level: &Level) -> &'static str {
    match *level {
        Level::TRACE => "trace",
        Level::DEBUG => "debug",
        Level::INFO => "info",
        Level::WARN => "warn",
        Level::ERROR => "error",
    }
}

fn rfc3339_now() -> String {
    let mut out = String::new();
    // Writing to a String cannot fail.
    let _ = SystemTime.format_time(&mut Writer::new(&mut out));
    out
}

impl<S, W> Layer<S> for JsonLogLayer<W>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'w> MakeWriter<'w> + 'static,
{
    fn on_register_dispatch(&self, subscriber: &Dispatch) {
        let _ = self.dispatch.set(subscriber.downgrade());
    }

    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut fields = SpanFields::default();
        attrs.record(&mut FieldVisitor(&mut fields.0));
        span.extensions_mut().insert(fields);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut extensions = span.extensions_mut();
        if let Some(fields) = extensions.get_mut::<SpanFields>() {
            values.record(&mut FieldVisitor(&mut fields.0));
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut out = Map::new();

        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                if let Some(fields) = span.extensions().get::<SpanFields>() {
                    for (key, value) in &fields.0 {
                        out.insert(key.clone(), value.clone());
                    }
                }
            }
        }

        let mut event_fields = Map::new();
        event.record(&mut FieldVisitor(&mut event_fields));
        let message = event_fields.remove("message");
        out.extend(event_fields);

        let metadata = event.metadata();
        out.insert("timestamp".to_owned(), Value::String(rfc3339_now()));
        out.insert(
            "level".to_owned(),
            Value::String(level_str(metadata.level()).to_owned()),
        );
        out.insert(
            "message".to_owned(),
            message.unwrap_or_else(|| Value::String(String::new())),
        );
        out.insert(
            "target".to_owned(),
            Value::String(metadata.target().to_owned()),
        );
        out.insert(
            "service".to_owned(),
            Value::String(self.tags.service.clone()),
        );
        match &self.tags.env {
            Some(env) => out.insert("env".to_owned(), Value::String(env.clone())),
            None => out.remove("env"),
        };
        out.insert(
            "version".to_owned(),
            Value::String(self.tags.version.clone()),
        );

        out.remove("dd.trace_id");
        out.remove("dd.span_id");
        if let Some((trace_id, span_id)) = self.dd_ids(&ctx, event) {
            out.insert("dd.trace_id".to_owned(), Value::String(trace_id));
            out.insert("dd.span_id".to_owned(), Value::String(span_id));
        }

        let Ok(mut line) = serde_json::to_vec(&Value::Object(out)) else {
            return;
        };
        line.push(b'\n');
        // A failed log write must never fail a sync.
        let _ = self.make_writer.make_writer().write_all(&line);
    }
}
