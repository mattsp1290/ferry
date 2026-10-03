//! Metrics, logs, and traces for the in-cluster Datadog Agent.

pub mod metrics;

pub use metrics::{METRIC_NAMES, Metrics, NoopMetrics, RecordingMetrics};
