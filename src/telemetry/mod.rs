//! Metrics, logs, and traces for the in-cluster Datadog Agent.
//!
//! Telemetry never fails a sync. A bad URL disables that signal with a warning;
//! a send or export error is counted and logged at most once per minute.
//!
//! # Lifecycle
//!
//! `Telemetry::init` installs the global `tracing` subscriber and returns a
//! `TelemetryGuard`. Keep the guard alive for the whole run and call
//! `TelemetryGuard::shutdown` (or drop it) before the process exits so the
//! last spans are flushed. Shutdown blocks the calling thread for up to
//! `tracing::SHUTDOWN_TIMEOUT`. The tracer provider runs on its own threads
//! and needs no tokio runtime, so it is safe to call from anywhere, but from
//! async code prefer `tokio::task::spawn_blocking` or do it after the runtime
//! has finished. DogStatsD sends are unbuffered, so metrics need no flush.

pub mod logging;
pub mod metrics;
pub mod tracing;

use std::io;
use std::sync::Arc;

use ::tracing::Dispatch;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Layer, Registry};

pub use logging::JsonLogLayer;
pub use metrics::{
    ConstTags, DogstatsdMetrics, DogstatsdTarget, METRIC_NAMES, Metrics, NoopMetrics,
    RecordingMetrics,
};

/// Log output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// One JSON object per line (production).
    #[default]
    Json,
    /// Human-readable lines (local use).
    Text,
}

impl LogFormat {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "json" => Some(Self::Json),
            "text" => Some(Self::Text),
            _ => None,
        }
    }
}

/// Telemetry configuration. Every field is public, so tests build one directly
/// (`Settings { dogstatsd_url: Some(..), ..Settings::default() }`) without
/// touching the process environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// `DD_SERVICE`, default `ferry`.
    pub service: String,
    /// `DD_ENV`, default none.
    pub env: Option<String>,
    /// `DD_VERSION`, default the crate version.
    pub version: String,
    /// `DD_DOGSTATSD_URL`: `udp://host:port` or `unix:///path`. None disables metrics.
    pub dogstatsd_url: Option<String>,
    /// `DD_TRACE_AGENT_URL`: `http://host:8126` or `unix:///path`. None disables tracing.
    pub trace_agent_url: Option<String>,
    /// `FERRY_LOG_FORMAT`, default `json`.
    pub log_format: LogFormat,
    /// `FERRY_LOG_LEVEL`, an env-filter directive, default `info`.
    pub log_level: String,
    /// Problems found while reading the environment; logged by `Telemetry::build`.
    pub warnings: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self::from_lookup(|_| None)
    }
}

impl Settings {
    /// Reads the process environment.
    pub fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Reads settings through `lookup` (variable name to value). Pure: tests
    /// pass a closure over a map. Empty values count as unset.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let get = |name: &str| {
            lookup(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let mut warnings = Vec::new();
        let log_format = match get("FERRY_LOG_FORMAT") {
            None => LogFormat::default(),
            Some(value) => LogFormat::parse(&value).unwrap_or_else(|| {
                warnings.push(format!(
                    "FERRY_LOG_FORMAT={value:?} is not `json` or `text`; using json"
                ));
                LogFormat::default()
            }),
        };
        Self {
            service: get("DD_SERVICE").unwrap_or_else(|| "ferry".to_owned()),
            env: get("DD_ENV"),
            version: get("DD_VERSION").unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned()),
            dogstatsd_url: get("DD_DOGSTATSD_URL"),
            trace_agent_url: get("DD_TRACE_AGENT_URL"),
            log_format,
            log_level: get("FERRY_LOG_LEVEL").unwrap_or_else(|| "info".to_owned()),
            warnings,
        }
    }

    fn const_tags(&self) -> ConstTags {
        ConstTags {
            service: self.service.clone(),
            env: self.env.clone(),
            version: self.version.clone(),
        }
    }
}

/// Entry point for telemetry setup.
pub struct Telemetry;

type Base = Registry;

/// Spans and span events at this level and above are exported as traces,
/// whatever the log level is.
const TRACE_LEVEL: LevelFilter = LevelFilter::INFO;

impl Telemetry {
    /// Installs the global subscriber (level filter, JSON or text logs on
    /// stdout, and the OpenTelemetry layer when tracing is enabled) and builds
    /// the metrics backend. Call once, early, inside the tokio runtime or not.
    ///
    /// If a global subscriber already exists, the existing one stays and a
    /// note goes to stderr; metrics and tracing still work.
    pub fn init(settings: Settings) -> TelemetryGuard {
        let (guard, dispatch) = Self::build(settings, io::stdout);
        if ::tracing::dispatcher::set_global_default(dispatch).is_err() {
            eprintln!("ferry: a global tracing subscriber is already installed; keeping it");
        }
        guard
    }

    /// Builds the guard and the subscriber without installing anything
    /// globally. `init` calls this with stdout; tests call it with a capturing
    /// writer and install the `Dispatch` with `tracing::dispatcher::with_default`.
    pub fn build<W>(settings: Settings, writer: W) -> (TelemetryGuard, Dispatch)
    where
        W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
    {
        let mut warnings = settings.warnings.clone();

        let filter = EnvFilter::try_new(&settings.log_level).unwrap_or_else(|error| {
            warnings.push(format!(
                "FERRY_LOG_LEVEL={:?} is not a valid filter ({error}); using info",
                settings.log_level
            ));
            EnvFilter::new("info")
        });

        let provider = settings.trace_agent_url.as_deref().and_then(|raw| {
            match tracing::parse_agent_url(raw) {
                Ok(url) => Some(tracing::build_datadog_provider(&settings, &url)),
                Err(error) => {
                    warnings.push(format!(
                        "DD_TRACE_AGENT_URL={raw:?} is invalid ({error}); tracing is disabled"
                    ));
                    None
                }
            }
        });

        let metrics: Option<Arc<dyn Metrics>> = settings.dogstatsd_url.as_deref().and_then(|raw| {
            let target = match DogstatsdTarget::parse(raw) {
                Ok(target) => target,
                Err(error) => {
                    warnings.push(format!(
                        "DD_DOGSTATSD_URL={raw:?} is invalid ({error}); metrics are disabled"
                    ));
                    return None;
                }
            };
            match DogstatsdMetrics::connect(&target, &settings.const_tags()) {
                Ok(metrics) => Some(Arc::new(metrics) as Arc<dyn Metrics>),
                Err(error) => {
                    warnings.push(format!(
                        "DogStatsD setup failed ({error}); metrics are disabled"
                    ));
                    None
                }
            }
        });

        let log_layer: Box<dyn Layer<Base> + Send + Sync> = match settings.log_format {
            LogFormat::Json => JsonLogLayer::new(
                writer,
                settings.service.clone(),
                settings.env.clone(),
                settings.version.clone(),
            )
            .boxed(),
            LogFormat::Text => tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .boxed(),
        };
        // The level filter belongs to the log layer alone. As a global filter
        // it would also drop the spans, and `FERRY_LOG_LEVEL=warn` would then
        // silently turn tracing off. Ferry exports every sync as a trace.
        let otel = provider
            .as_ref()
            .map(|provider| tracing::otel_layer(provider).with_filter(TRACE_LEVEL));
        let subscriber = tracing_subscriber::registry()
            .with(log_layer.with_filter(filter))
            .with(otel);
        let dispatch = Dispatch::new(subscriber);

        ::tracing::dispatcher::with_default(&dispatch, || {
            for warning in &warnings {
                ::tracing::warn!("telemetry: {warning}");
            }
        });

        let guard = TelemetryGuard {
            metrics_enabled: metrics.is_some(),
            metrics: metrics.unwrap_or_else(|| Arc::new(NoopMetrics)),
            provider,
        };
        (guard, dispatch)
    }
}

/// Owns the telemetry backends. Dropping it shuts them down.
pub struct TelemetryGuard {
    metrics: Arc<dyn Metrics>,
    metrics_enabled: bool,
    provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

impl std::fmt::Debug for TelemetryGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryGuard")
            .field("metrics_enabled", &self.metrics_enabled)
            .field("tracing_enabled", &self.provider.is_some())
            .finish()
    }
}

impl TelemetryGuard {
    /// The metrics backend: DogStatsD when configured, otherwise a no-op.
    pub fn metrics(&self) -> Arc<dyn Metrics> {
        Arc::clone(&self.metrics)
    }

    /// True when `DD_DOGSTATSD_URL` was set and valid.
    pub fn metrics_enabled(&self) -> bool {
        self.metrics_enabled
    }

    /// True when `DD_TRACE_AGENT_URL` was set and valid.
    pub fn tracing_enabled(&self) -> bool {
        self.provider.is_some()
    }

    /// The tracer provider, when tracing is enabled.
    pub fn tracer_provider(&self) -> Option<&opentelemetry_sdk::trace::SdkTracerProvider> {
        self.provider.as_ref()
    }

    /// Flushes and shuts down the tracer provider. Idempotent. Blocks up to
    /// `tracing::SHUTDOWN_TIMEOUT`; see the module docs.
    pub fn shutdown(&mut self) {
        if let Some(provider) = self.provider.take()
            && let Err(error) = provider.shutdown_with_timeout(tracing::SHUTDOWN_TIMEOUT)
        {
            ::tracing::warn!(error = %error, "telemetry: tracer shutdown did not complete cleanly");
        }
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        self.shutdown();
    }
}
