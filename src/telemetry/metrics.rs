//! The metrics interface between the sync engine and the telemetry backend.
//!
//! The sync engine and scheduler depend only on the `Metrics` trait. Metric
//! names are a contract with the files in `datadog/`: `METRIC_NAMES` lists
//! every name, and `tests/datadog_assets.rs` checks the queries against it.

use std::sync::Mutex;
use std::time::Duration;

use crate::config::RepoEntry;
use crate::sync::outcome::SyncOutcome;
use crate::util::lock;

pub const SYNC_RUNS: &str = "ferry.sync.runs";
pub const SYNC_DURATION: &str = "ferry.sync.duration";
pub const SYNC_REFS_CHANGED: &str = "ferry.sync.refs_changed";
pub const SYNC_REFS_PRUNED: &str = "ferry.sync.refs_pruned";
pub const REPO_LAST_SUCCESS_AGE: &str = "ferry.repo.last_success_age_seconds";
pub const REPO_CONSECUTIVE_FAILURES: &str = "ferry.repo.consecutive_failures";
pub const REPOS_CONFIGURED: &str = "ferry.repos.configured";
pub const CACHE_BYTES: &str = "ferry.cache.bytes";
pub const HEARTBEAT: &str = "ferry.heartbeat";

/// Every metric name ferry emits.
pub const METRIC_NAMES: [&str; 9] = [
    SYNC_RUNS,
    SYNC_DURATION,
    SYNC_REFS_CHANGED,
    SYNC_REFS_PRUNED,
    REPO_LAST_SUCCESS_AGE,
    REPO_CONSECUTIVE_FAILURES,
    REPOS_CONFIGURED,
    CACHE_BYTES,
    HEARTBEAT,
];

/// Telemetry never fails a sync, so no method returns an error.
pub trait Metrics: Send + Sync {
    /// One finished repository sync.
    fn sync_finished(&self, entry: &RepoEntry, outcome: &SyncOutcome);
    /// Periodic per-entry state: age of the last success and failure streak.
    fn repo_state(&self, entry: &RepoEntry, last_success_age: Duration, consecutive_failures: u32);
    /// Allowlist size.
    fn repos_configured(&self, count: usize);
    /// Disk use of the cache directory.
    fn cache_bytes(&self, bytes: u64);
    /// The scheduler loop is alive.
    fn heartbeat(&self);
}

/// Used when `DD_DOGSTATSD_URL` is unset.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopMetrics;

impl Metrics for NoopMetrics {
    fn sync_finished(&self, _entry: &RepoEntry, _outcome: &SyncOutcome) {}
    fn repo_state(&self, _entry: &RepoEntry, _age: Duration, _failures: u32) {}
    fn repos_configured(&self, _count: usize) {}
    fn cache_bytes(&self, _bytes: u64) {}
    fn heartbeat(&self) {}
}

/// One call recorded by `RecordingMetrics`.
#[derive(Debug, Clone, PartialEq)]
pub enum MetricEvent {
    SyncFinished {
        repo: String,
        outcome: SyncOutcome,
    },
    RepoState {
        repo: String,
        last_success_age: Duration,
        consecutive_failures: u32,
    },
    ReposConfigured(usize),
    CacheBytes(u64),
    Heartbeat,
}

/// Records every call in order. Tests assert on the recorded events.
#[derive(Debug, Default)]
pub struct RecordingMetrics {
    events: Mutex<Vec<MetricEvent>>,
}

impl RecordingMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn events(&self) -> Vec<MetricEvent> {
        self.lock().clone()
    }

    /// The outcomes of finished syncs for one `repo` tag, in order.
    pub fn outcomes_for(&self, repo: &str) -> Vec<SyncOutcome> {
        self.lock()
            .iter()
            .filter_map(|event| match event {
                MetricEvent::SyncFinished { repo: r, outcome } if r == repo => {
                    Some(outcome.clone())
                }
                _ => None,
            })
            .collect()
    }

    pub fn heartbeat_count(&self) -> usize {
        self.lock()
            .iter()
            .filter(|event| matches!(event, MetricEvent::Heartbeat))
            .count()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<MetricEvent>> {
        // A panicking test thread must not hide the events recorded so far.
        lock(&self.events)
    }
}

impl Metrics for RecordingMetrics {
    fn sync_finished(&self, entry: &RepoEntry, outcome: &SyncOutcome) {
        self.lock().push(MetricEvent::SyncFinished {
            repo: entry.repo_tag(),
            outcome: outcome.clone(),
        });
    }

    fn repo_state(&self, entry: &RepoEntry, last_success_age: Duration, consecutive_failures: u32) {
        self.lock().push(MetricEvent::RepoState {
            repo: entry.repo_tag(),
            last_success_age,
            consecutive_failures,
        });
    }

    fn repos_configured(&self, count: usize) {
        self.lock().push(MetricEvent::ReposConfigured(count));
    }

    fn cache_bytes(&self, bytes: u64) {
        self.lock().push(MetricEvent::CacheBytes(bytes));
    }

    fn heartbeat(&self) {
        self.lock().push(MetricEvent::Heartbeat);
    }
}
