//! Capturing log output and exported spans in tests.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use ferry::telemetry::logging::JsonLogLayer;
use ferry::telemetry::tracing::{OperationNameProcessor, otel_layer};
use opentelemetry::Value;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use tracing::Dispatch;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;

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

/// A dispatch with the JSON log layer and the OTel layer over `provider`.
pub fn json_dispatch(capture: &Capture, provider: &SdkTracerProvider) -> Dispatch {
    Dispatch::new(
        tracing_subscriber::registry()
            .with(JsonLogLayer::new(
                capture.clone(),
                "ferry".to_owned(),
                Some("test".to_owned()),
                "1.2.3".to_owned(),
            ))
            .with(otel_layer(provider)),
    )
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
