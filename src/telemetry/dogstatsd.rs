//! DogStatsD backend of the `Metrics` interface.

use std::fmt;
use std::io;
use std::net::{ToSocketAddrs, UdpSocket};
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cadence::{
    Counted, Distributed, Gauged, MetricError, MetricSink, StatsdClient, StatsdClientBuilder,
    UnixMetricSink,
};

use super::Throttle;
use super::metrics::{
    CACHE_BYTES, HEARTBEAT, Metrics, REPO_CONSECUTIVE_FAILURES, REPO_LAST_SUCCESS_AGE,
    REPOS_CONFIGURED, SYNC_DURATION, SYNC_REFS_CHANGED, SYNC_REFS_PRUNED, SYNC_RUNS,
};
use crate::config::RepoEntry;
use crate::sync::outcome::SyncOutcome;
use crate::util::lock;

/// How often a `udp://` sink re-resolves its host and re-creates its socket.
///
/// A Service-routed UDP flow can stay pinned to a dead Agent pod through a
/// stale conntrack entry. A new socket has a new source port, hence a new
/// flow, and a fresh DNS answer follows a rescheduled Agent.
pub const UDP_ROTATION_INTERVAL: Duration = Duration::from_secs(60);

/// Minimum spacing of resolution attempts while no address is known yet.
const RESOLVE_RETRY: Duration = Duration::from_secs(5);

/// Send errors are logged at most this often.
const ERROR_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// Where DogStatsD datagrams go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DogstatsdTarget {
    /// `udp://host:port`. `host_port` is `host:port`, port defaulting to 8125.
    Udp { host_port: String },
    /// `unix:///path`, a datagram socket.
    Unix { path: PathBuf },
}

impl DogstatsdTarget {
    /// Parses `udp://host[:port]` or `unix:///path`.
    pub fn parse(value: &str) -> Result<Self, String> {
        let url = url::Url::parse(value.trim()).map_err(|e| format!("not a valid URL: {e}"))?;
        match url.scheme() {
            "udp" => {
                let host = url.host_str().filter(|h| !h.is_empty());
                let host = host.ok_or("udp URL needs a host")?;
                let port = url.port().unwrap_or(8125);
                Ok(Self::Udp {
                    host_port: format!("{host}:{port}"),
                })
            }
            "unix" => {
                let path = url.path();
                if path.is_empty() || path == "/" {
                    return Err("unix URL needs a socket path (unix:///path)".to_owned());
                }
                Ok(Self::Unix {
                    path: PathBuf::from(path),
                })
            }
            other => Err(format!(
                "unsupported scheme `{other}` (use udp://host:port or unix:///path)"
            )),
        }
    }
}

/// Tags attached to every metric. `env` is omitted when unset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstTags {
    pub service: String,
    pub env: Option<String>,
    pub version: String,
}

/// Counts send errors and logs the first one in every minute.
#[derive(Debug)]
pub struct SendErrors {
    count: AtomicU64,
    throttle: Throttle,
}

impl Default for SendErrors {
    fn default() -> Self {
        Self {
            count: AtomicU64::new(0),
            throttle: Throttle::new(ERROR_LOG_INTERVAL),
        }
    }
}

impl SendErrors {
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    fn record(&self, what: &dyn fmt::Display) {
        self.count.fetch_add(1, Ordering::Relaxed);
        if self.throttle.ready() {
            tracing::warn!(
                error = %what,
                total_errors = self.count(),
                "dogstatsd send failed; further errors are counted, not logged, for 60s"
            );
        }
    }
}

struct UdpConn {
    socket: UdpSocket,
    addr: std::net::SocketAddr,
}

impl UdpConn {
    fn open(host_port: &str) -> io::Result<Self> {
        let addr = host_port.to_socket_addrs()?.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "host resolved to no address")
        })?;
        let bind = if addr.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let socket = UdpSocket::bind(bind)?;
        socket.set_nonblocking(true)?;
        Ok(Self { socket, addr })
    }
}

struct UdpState {
    conn: Option<Arc<UdpConn>>,
    last_attempt: Instant,
}

struct UdpShared {
    host_port: String,
    interval: Duration,
    state: Mutex<UdpState>,
    errors: Arc<SendErrors>,
}

impl UdpShared {
    fn lock(&self) -> std::sync::MutexGuard<'_, UdpState> {
        lock(&self.state)
    }
}

/// A UDP sink that periodically swaps in a freshly resolved, freshly bound
/// socket. Sends never block on DNS: the refresh runs on a short-lived thread
/// while sends continue on the old socket.
struct RotatingUdpSink {
    shared: Arc<UdpShared>,
}

impl RotatingUdpSink {
    fn new(
        host_port: String,
        interval: Duration,
        errors: Arc<SendErrors>,
    ) -> (Self, Option<String>) {
        let (conn, warning) = match UdpConn::open(&host_port) {
            Ok(conn) => (Some(Arc::new(conn)), None),
            Err(error) => (
                None,
                Some(format!("DogStatsD initial resolve failed: {error}")),
            ),
        };
        (
            Self {
                shared: Arc::new(UdpShared {
                    host_port,
                    interval,
                    state: Mutex::new(UdpState {
                        conn,
                        last_attempt: Instant::now(),
                    }),
                    errors,
                }),
            },
            warning,
        )
    }

    /// Returns the connection to send on and starts a refresh when one is due.
    fn current(&self) -> Option<Arc<UdpConn>> {
        let shared = &self.shared;
        let mut state = shared.lock();
        let wait = if state.conn.is_some() {
            shared.interval
        } else {
            shared.interval.min(RESOLVE_RETRY)
        };
        if state.last_attempt.elapsed() >= wait {
            state.last_attempt = Instant::now();
            let worker = Arc::clone(shared);
            let spawned = std::thread::Builder::new()
                .name("ferry-dogstatsd-resolve".to_owned())
                .spawn(move || match UdpConn::open(&worker.host_port) {
                    Ok(conn) => worker.lock().conn = Some(Arc::new(conn)),
                    Err(error) => worker
                        .errors
                        .record(&format_args!("re-resolve of {}: {error}", worker.host_port)),
                });
            if let Err(error) = spawned {
                shared.errors.record(&error);
            }
        }
        state.conn.clone()
    }
}

impl MetricSink for RotatingUdpSink {
    fn emit(&self, metric: &str) -> io::Result<usize> {
        let conn = self.current().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "dogstatsd host not resolved")
        })?;
        conn.socket.send_to(metric.as_bytes(), conn.addr)
    }
}

/// DogStatsD backend. Every metric carries `service`, `env` (when set), and
/// `version`. Send errors never propagate: they are counted and logged at most
/// once per minute.
///
/// `sync_finished` always emits all four of runs, duration, refs_changed, and
/// refs_pruned (the two ref counts may be 0).
pub struct DogstatsdMetrics {
    client: StatsdClient,
    errors: Arc<SendErrors>,
}

impl fmt::Debug for DogstatsdMetrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DogstatsdMetrics")
            .field("send_errors", &self.errors.count())
            .finish_non_exhaustive()
    }
}

impl DogstatsdMetrics {
    /// Connects with the production rotation interval (`UDP_ROTATION_INTERVAL`).
    pub fn connect(target: &DogstatsdTarget, tags: &ConstTags) -> io::Result<Self> {
        Self::connect_with_initial_warning(target, tags).map(|(metrics, _)| metrics)
    }

    /// Like `connect` with an explicit UDP rotation interval (tests use a short one).
    /// The interval is ignored for Unix sockets.
    pub fn connect_with_rotation(
        target: &DogstatsdTarget,
        tags: &ConstTags,
        rotation: Duration,
    ) -> io::Result<Self> {
        Self::connect_with_warning(target, tags, rotation).map(|(metrics, _)| metrics)
    }

    /// Keeps the initial DNS failure available until a logging subscriber exists.
    pub fn connect_with_initial_warning(
        target: &DogstatsdTarget,
        tags: &ConstTags,
    ) -> io::Result<(Self, Option<String>)> {
        Self::connect_with_warning(target, tags, UDP_ROTATION_INTERVAL)
    }

    fn connect_with_warning(
        target: &DogstatsdTarget,
        tags: &ConstTags,
        rotation: Duration,
    ) -> io::Result<(Self, Option<String>)> {
        let errors = Arc::new(SendErrors::default());
        let mut warning = None;
        let builder = match target {
            DogstatsdTarget::Udp { host_port } => {
                let (sink, initial_warning) =
                    RotatingUdpSink::new(host_port.clone(), rotation, Arc::clone(&errors));
                warning = initial_warning;
                StatsdClient::builder("", sink)
            }
            DogstatsdTarget::Unix { path } => {
                let socket = UnixDatagram::unbound()?;
                socket.set_nonblocking(true)?;
                StatsdClient::builder("", UnixMetricSink::from(path, socket))
            }
        };
        Ok((
            Self {
                client: Self::finish(builder, tags, &errors),
                errors,
            },
            warning,
        ))
    }

    fn finish(
        builder: StatsdClientBuilder,
        tags: &ConstTags,
        errors: &Arc<SendErrors>,
    ) -> StatsdClient {
        let handler = Arc::clone(errors);
        let mut builder = builder
            .with_error_handler(move |error: MetricError| handler.record(&error))
            .with_tag("service", &tags.service);
        if let Some(env) = &tags.env {
            builder = builder.with_tag("env", env);
        }
        builder.with_tag("version", &tags.version).build()
    }

    /// Total send errors since start.
    pub fn send_error_count(&self) -> u64 {
        self.errors.count()
    }
}

impl Metrics for DogstatsdMetrics {
    fn sync_finished(&self, entry: &RepoEntry, outcome: &SyncOutcome) {
        let repo = entry.repo_tag();
        let result = outcome.result.as_str();
        self.client
            .count_with_tags(SYNC_RUNS, 1_u64)
            .with_tag("repo", &repo)
            .with_tag("result", result)
            .with_tag("error_kind", outcome.error_kind_tag())
            .send();
        self.client
            .distribution_with_tags(SYNC_DURATION, outcome.duration.as_secs_f64())
            .with_tag("repo", &repo)
            .with_tag("result", result)
            .send();
        self.client
            .count_with_tags(SYNC_REFS_CHANGED, u64::from(outcome.refs_changed))
            .with_tag("repo", &repo)
            .send();
        self.client
            .count_with_tags(SYNC_REFS_PRUNED, u64::from(outcome.refs_pruned))
            .with_tag("repo", &repo)
            .send();
    }

    fn repo_state(&self, entry: &RepoEntry, last_success_age: Duration, consecutive_failures: u32) {
        let repo = entry.repo_tag();
        self.client
            .gauge_with_tags(REPO_LAST_SUCCESS_AGE, last_success_age.as_secs_f64())
            .with_tag("repo", &repo)
            .send();
        self.client
            .gauge_with_tags(REPO_CONSECUTIVE_FAILURES, u64::from(consecutive_failures))
            .with_tag("repo", &repo)
            .send();
    }

    fn repos_configured(&self, count: usize) {
        self.client
            .gauge_with_tags(REPOS_CONFIGURED, count as u64)
            .send();
    }

    fn cache_bytes(&self, bytes: u64) {
        self.client.gauge_with_tags(CACHE_BYTES, bytes).send();
    }

    fn heartbeat(&self) {
        self.client.gauge_with_tags(HEARTBEAT, 1_u64).send();
    }
}

#[cfg(test)]
mod dogstatsd_tests {
    use super::*;

    #[test]
    fn parses_targets() {
        assert_eq!(
            DogstatsdTarget::parse("udp://agent.local:9999").unwrap(),
            DogstatsdTarget::Udp {
                host_port: "agent.local:9999".into()
            }
        );
        assert_eq!(
            DogstatsdTarget::parse("udp://agent.local").unwrap(),
            DogstatsdTarget::Udp {
                host_port: "agent.local:8125".into()
            }
        );
        assert_eq!(
            DogstatsdTarget::parse("unix:///var/run/dsd.sock").unwrap(),
            DogstatsdTarget::Unix {
                path: "/var/run/dsd.sock".into()
            }
        );
        for bad in ["", "agent:8125", "http://agent:8125", "unix://", "udp://:1"] {
            assert!(DogstatsdTarget::parse(bad).is_err(), "{bad:?} must fail");
        }
    }
}
