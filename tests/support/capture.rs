//! Capturing log output and exported spans in tests.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use ferry::telemetry::ConstTags;
use ferry::telemetry::logging::JsonLogLayer;
use ferry::telemetry::tracing::OperationNameProcessor;
use opentelemetry::Value;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use tracing::Dispatch;
use tracing::dispatcher::DefaultGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

/// A `MakeWriter` that appends to a shared buffer.
#[derive(Clone, Default)]
pub struct Capture(Arc<Mutex<Vec<u8>>>);

pub struct CaptureHandle(Arc<Mutex<Vec<u8>>>);

impl Write for CaptureHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = CaptureHandle;

    fn make_writer(&'a self) -> Self::Writer {
        CaptureHandle(Arc::clone(&self.0))
    }
}

impl Capture {
    pub fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    pub fn json_lines(&self) -> Vec<serde_json::Value> {
        self.text()
            .lines()
            .map(|line| serde_json::from_str(line).expect("every log line is JSON"))
            .collect()
    }
}

pub fn memory_provider() -> (SdkTracerProvider, InMemorySpanExporter) {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_span_processor(OperationNameProcessor)
        .with_simple_exporter(exporter.clone())
        .build();
    (provider, exporter)
}

/// A dispatch with the JSON log layer and the OTel layer over `provider`,
/// assembled exactly as production does, with the log level at `debug`.
pub fn json_dispatch(capture: &Capture, provider: &SdkTracerProvider) -> Dispatch {
    json_dispatch_at(capture, provider, "debug")
}

/// Keeps a scoped subscriber installed, and the tracer provider behind it
/// alive, for as long as it is held. Bind it to a name, not `_`.
pub struct ScopedSubscriber {
    _guard: DefaultGuard,
    _provider: SdkTracerProvider,
}

/// Installs `json_dispatch` over a fresh in-memory provider for the current
/// thread. Returns the log capture, the span exporter, and the guard.
pub fn scoped_json_capture() -> (Capture, InMemorySpanExporter, ScopedSubscriber) {
    let capture = Capture::default();
    let (provider, exporter) = memory_provider();
    let guard = tracing::dispatcher::set_default(&json_dispatch(&capture, &provider));
    (
        capture,
        exporter,
        ScopedSubscriber {
            _guard: guard,
            _provider: provider,
        },
    )
}

/// Like `json_dispatch` with an explicit `FERRY_LOG_LEVEL` directive.
pub fn json_dispatch_at(capture: &Capture, provider: &SdkTracerProvider, level: &str) -> Dispatch {
    ferry::telemetry::compose(
        JsonLogLayer::new(capture.clone(), tags()),
        EnvFilter::new(level),
        Some(provider),
    )
}

/// The constant tags every telemetry test uses.
pub fn tags() -> ConstTags {
    ConstTags {
        service: "ferry".to_owned(),
        env: Some("test".to_owned()),
        version: "1.2.3".to_owned(),
    }
}

pub fn attr(span: &SpanData, key: &str) -> Option<Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.clone())
}

pub fn attr_str(span: &SpanData, key: &str) -> Option<String> {
    attr(span, key).map(|v| v.as_str().into_owned())
}
