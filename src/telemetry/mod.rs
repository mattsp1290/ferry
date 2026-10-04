//! Metrics, logs, and traces for the in-cluster Datadog Agent.
//!
//! Telemetry never fails a sync. A bad URL disables that signal with a warning.
//! A DogStatsD send error is counted and logged at most once per minute, and
//! the Datadog trace exporter's own error lines are limited to one per minute.
//!
//! # Lifecycle
//!
//! `init` installs the global `tracing` subscriber and returns a
//! `TelemetryGuard`. Keep the guard alive for the whole run and call
//! `TelemetryGuard::shutdown` (or drop it) before the process exits so the
//! last spans are flushed. Shutdown blocks the calling thread for up to
//! `tracing::SHUTDOWN_TIMEOUT`. The tracer provider runs on its own threads
//! and needs no tokio runtime, so it is safe to call from anywhere, but from
//! async code prefer `tokio::task::spawn_blocking` or do it after the runtime
//! has finished. DogStatsD sends are unbuffered, so metrics need no flush.

pub mod dogstatsd;
pub mod logging;
pub mod metrics;
pub mod tracing;

use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ::tracing::Dispatch;
use tracing_subscriber::filter::{FilterExt, LevelFilter, dynamic_filter_fn, filter_fn};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Layer, Registry};

use crate::util::lock;

pub use dogstatsd::{ConstTags, DogstatsdMetrics, DogstatsdTarget};
pub use logging::JsonLogLayer;
pub use metrics::{METRIC_NAMES, Metrics, NoopMetrics, RecordingMetrics};

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
    /// Problems found while reading the environment; logged by `build`.
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

type Base = Registry;

/// Assembles the subscriber: the log layer behind the level filter, and the
/// trace layer when a provider exists. `build` uses it, and tests
/// use it with an in-memory provider so that they exercise the same layering.
///
/// The level filter belongs to the log layer alone. As a global filter it
/// would also drop the spans, and `FERRY_LOG_LEVEL=warn` would then silently
/// turn tracing off. Within the log layer it applies to events only: spans
/// always reach the layer, because it takes `repo` and the trace ID of a log
/// line from the enclosing spans. Otherwise an error line above `info` would
/// not say which repository failed.
pub fn compose<L>(
    log_layer: L,
    level: EnvFilter,
    provider: Option<&opentelemetry_sdk::trace::SdkTracerProvider>,
) -> Dispatch
where
    L: Layer<Base> + Send + Sync + 'static,
{
    let spans = filter_fn(|meta| meta.is_span() && *meta.level() <= ::tracing::Level::INFO);
    let log_filter = spans
        .or(level)
        // Dynamic: a plain `filter_fn` is evaluated once per callsite and
        // cached, which would turn the throttle into "always" or "never".
        .and(dynamic_filter_fn(exporter_throttle(EXPORTER_LOG_INTERVAL)));
    let otel = provider.map(|provider| tracing::otel_layer(provider).with_filter(TRACE_LEVEL));
    Dispatch::new(
        tracing_subscriber::registry()
            .with(log_layer.with_filter(log_filter))
            .with(otel),
    )
}

/// The Datadog exporter crates log every failed export at `error`. With the
/// Agent down that is several lines per sync, which would bury real sync
/// failures in the error stream.
const EXPORTER_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// Lets one event per `interval` through from the Datadog exporter crates and
/// everything from other targets.
fn exporter_throttle<S>(
    interval: Duration,
) -> impl Fn(&::tracing::Metadata<'_>, &tracing_subscriber::layer::Context<'_, S>) -> bool {
    let throttle = Throttle::new(interval);
    move |meta, _| {
        let from_exporter = meta.target().starts_with("libdd_")
            || meta.target().starts_with("datadog_opentelemetry");
        if !from_exporter || meta.is_span() {
            return true;
        }
        throttle.ready()
    }
}

/// Lets one call per `interval` through.
#[derive(Debug)]
pub(crate) struct Throttle {
    interval: Duration,
    last: Mutex<Option<Instant>>,
}

impl Throttle {
    pub(crate) fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: Mutex::new(None),
        }
    }

    /// True for the first call and then at most once per interval.
    pub(crate) fn ready(&self) -> bool {
        let mut last = lock(&self.last);
        let due = last.is_none_or(|at| at.elapsed() >= self.interval);
        if due {
            *last = Some(Instant::now());
        }
        due
    }
}

/// Spans and span events at this level and above are exported as traces,
/// whatever the log level is.
const TRACE_LEVEL: LevelFilter = LevelFilter::INFO;

/// Installs the global subscriber (level filter, JSON or text logs on
/// stdout, and the OpenTelemetry layer when tracing is enabled) and builds
/// the metrics backend. Call once, early, inside the tokio runtime or not.
///
/// If a global subscriber already exists, the existing one stays and a
/// note goes to stderr; metrics and tracing still work.
pub fn init(settings: Settings) -> TelemetryGuard {
    let (guard, dispatch) = build(settings, io::stdout);
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
    let tags = settings.const_tags();

    let filter = EnvFilter::try_new(&settings.log_level).unwrap_or_else(|error| {
        warnings.push(format!(
            "FERRY_LOG_LEVEL={:?} is not a valid filter ({error}); using info",
            settings.log_level
        ));
        EnvFilter::new("info")
    });

    let provider =
        settings
            .trace_agent_url
            .as_deref()
            .and_then(|raw| match tracing::parse_agent_url(raw) {
                Ok(url) => Some(tracing::build_datadog_provider(&tags, &url)),
                Err(error) => {
                    warnings.push(format!(
                        "DD_TRACE_AGENT_URL={raw:?} is invalid ({error}); tracing is disabled"
                    ));
                    None
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
            match DogstatsdMetrics::connect(&target, &tags, dogstatsd::UDP_ROTATION_INTERVAL) {
                Ok((metrics, warning)) => {
                    warnings.extend(warning);
                    Some(Arc::new(metrics) as Arc<dyn Metrics>)
                },
                Err(error) => {
                    warnings.push(format!(
                        "DogStatsD setup failed (cannot create DogStatsD socket: {error}); metrics are disabled"
                    ));
                    None
                }
            }
        });

    let log_layer: Box<dyn Layer<Base> + Send + Sync> = match settings.log_format {
        LogFormat::Json => JsonLogLayer::new(writer, tags.clone()).boxed(),
        LogFormat::Text => tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_ansi(false)
            .boxed(),
    };
    let dispatch = compose(log_layer, filter, provider.as_ref());

    ::tracing::dispatcher::with_default(&dispatch, || {
        for warning in &warnings {
            ::tracing::warn!("telemetry: {warning}");
        }
    });

    let guard = TelemetryGuard { metrics, provider };
    (guard, dispatch)
}

/// Owns the telemetry backends. Dropping it shuts them down.
pub struct TelemetryGuard {
    metrics: Option<Arc<dyn Metrics>>,
    provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

impl std::fmt::Debug for TelemetryGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryGuard")
            .field("metrics_enabled", &self.metrics.is_some())
            .field("tracing_enabled", &self.provider.is_some())
            .finish()
    }
}

impl TelemetryGuard {
    /// The metrics backend: DogStatsD when configured, otherwise a no-op.
    pub fn metrics(&self) -> Arc<dyn Metrics> {
        self.metrics
            .clone()
            .unwrap_or_else(|| Arc::new(NoopMetrics))
    }

    /// True when `DD_DOGSTATSD_URL` was set and valid.
    pub fn metrics_enabled(&self) -> bool {
        self.metrics.is_some()
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
