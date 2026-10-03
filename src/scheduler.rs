//! Polling loop: decides when each allowlist entry syncs.
//!
//! The scheduler is one `select!` loop. Syncs run on spawned tasks, so a long
//! sync never delays the tick, and the tick is the only writer of the
//! scheduler heartbeat. A wedged loop therefore stops the heartbeat and fails
//! liveness even while other tasks keep running.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use tokio::task::{Id, JoinError, JoinSet};
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::config::{Config, RepoEntry};
use crate::health::HealthState;
use crate::sync::{ErrorKind, SyncOutcome};
use crate::telemetry::Metrics;

/// Upper bound of the failure backoff.
pub const MAX_BACKOFF: Duration = Duration::from_secs(3600);
/// Fraction by which a backoff delay is randomly shortened or lengthened.
const JITTER_FRACTION: f64 = 0.10;
const READY_RETRY_INITIAL: Duration = Duration::from_secs(5);
const READY_RETRY_MAX: Duration = Duration::from_secs(60);

/// What the scheduler needs from the sync engine. Tests script it.
#[async_trait]
pub trait Syncer: Send + Sync + 'static {
    /// Startup check that needs the network: the Forgejo token works.
    async fn check_ready(&self) -> Result<(), String>;
    /// One pass over one entry. Never panics on a sync failure; the failure
    /// is in the outcome.
    async fn sync(&self, entry: &RepoEntry) -> SyncOutcome;
}

#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    pub poll_interval: Duration,
    pub max_concurrency: usize,
    /// Loop tick: heartbeat, start due syncs.
    pub tick: Duration,
    /// How long shutdown waits for in-flight syncs before cancelling them.
    pub shutdown_grace: Duration,
    /// How long shutdown then waits for cancelled syncs to stop their git
    /// children (SIGTERM, then SIGKILL after the git runner's own grace).
    pub cancel_grace: Duration,
}

impl SchedulerConfig {
    pub fn from_config(config: &Config) -> Self {
        Self {
            poll_interval: config.sync.poll_interval(),
            max_concurrency: config.sync.max_concurrency,
            tick: Duration::from_secs(10),
            shutdown_grace: Duration::from_secs(25),
            cancel_grace: Duration::from_secs(12),
        }
    }
}

/// Per-entry state that the metrics emitter reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EntryStatus {
    pub last_success: Option<Instant>,
    pub consecutive_failures: u32,
}

/// Scheduler state shared with the emitter task. Indexed like the allowlist.
#[derive(Debug, Clone)]
pub struct SharedStatus {
    entries: Arc<[RepoEntry]>,
    statuses: Arc<Mutex<Vec<EntryStatus>>>,
    started: Instant,
}

impl SharedStatus {
    pub fn new(entries: &[RepoEntry]) -> Self {
        Self {
            entries: entries.into(),
            statuses: Arc::new(Mutex::new(vec![EntryStatus::default(); entries.len()])),
            started: Instant::now(),
        }
    }

    pub fn entries(&self) -> &[RepoEntry] {
        &self.entries
    }

    pub fn snapshot(&self) -> Vec<EntryStatus> {
        self.lock().clone()
    }

    /// Seconds since the last success. Before the first success of the
    /// process the age counts from process start: the previous success time
    /// is unknown, and counting from start delays a stale alert across a
    /// restart without ever suppressing it.
    pub fn last_success_age(&self, status: &EntryStatus, now: Instant) -> Duration {
        now.saturating_duration_since(status.last_success.unwrap_or(self.started))
    }

    fn update(&self, index: usize, status: EntryStatus) {
        self.lock()[index] = status;
    }

    fn lock(&self) -> MutexGuard<'_, Vec<EntryStatus>> {
        self.statuses.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct EntryState {
    next_attempt: Instant,
    in_flight: bool,
    status: EntryStatus,
}

struct Running {
    index: usize,
    started: Instant,
}

enum Finished {
    Ready(Result<(), String>),
    Sync(SyncOutcome),
}

pub struct Scheduler {
    syncer: Arc<dyn Syncer>,
    metrics: Arc<dyn Metrics>,
    health: HealthState,
    config: SchedulerConfig,
    shared: SharedStatus,
    states: Vec<EntryState>,
    tasks: JoinSet<Finished>,
    running: HashMap<Id, Running>,
    ready: bool,
    ready_check_in_flight: bool,
    ready_failures: u32,
    next_ready_attempt: Instant,
    jitter: Box<dyn FnMut() -> f64 + Send>,
}

impl Scheduler {
    pub fn new(
        syncer: Arc<dyn Syncer>,
        metrics: Arc<dyn Metrics>,
        health: HealthState,
        config: SchedulerConfig,
        shared: SharedStatus,
    ) -> Self {
        let now = Instant::now();
        let states = shared
            .entries()
            .iter()
            .map(|_| EntryState {
                next_attempt: now,
                in_flight: false,
                status: EntryStatus::default(),
            })
            .collect();
        Self {
            syncer,
            metrics,
            health,
            config,
            shared,
            states,
            tasks: JoinSet::new(),
            running: HashMap::new(),
            ready: false,
            ready_check_in_flight: false,
            ready_failures: 0,
            next_ready_attempt: now,
            jitter: Box::new(|| fastrand::f64() * 2.0 - 1.0),
        }
    }

    /// Replaces the jitter source. It returns a value in `-1.0..=1.0`, which
    /// is scaled to ±10 % of the backoff delay.
    pub fn with_jitter(mut self, jitter: impl FnMut() -> f64 + Send + 'static) -> Self {
        self.jitter = Box::new(jitter);
        self
    }

    /// Runs until `shutdown` is cancelled, then drains.
    ///
    /// `cancel_syncs` is the token the git runner watches. The scheduler
    /// cancels it when in-flight syncs outlive `shutdown_grace`, which makes
    /// the runner kill its child process groups.
    pub async fn run(mut self, shutdown: CancellationToken, cancel_syncs: CancellationToken) {
        let mut ticker = tokio::time::interval(self.config.tick);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                _ = ticker.tick() => {
                    self.health.beat();
                    self.start_ready_check();
                    self.start_due();
                }
                Some(joined) = self.tasks.join_next_with_id() => self.finish(joined),
            }
        }

        self.drain(&mut ticker, cancel_syncs).await;
    }

    fn start_ready_check(&mut self) {
        if self.ready || self.ready_check_in_flight || Instant::now() < self.next_ready_attempt {
            return;
        }
        let syncer = Arc::clone(&self.syncer);
        self.tasks
            .spawn(async move { Finished::Ready(syncer.check_ready().await) });
        self.ready_check_in_flight = true;
    }

    /// Starts due entries in config order, up to the concurrency limit.
    /// No sync starts before the first successful readiness check.
    fn start_due(&mut self) {
        if !self.ready {
            return;
        }
        let now = Instant::now();
        for index in 0..self.states.len() {
            if self.running.len() >= self.config.max_concurrency {
                break;
            }
            let state = &mut self.states[index];
            if state.in_flight || now < state.next_attempt {
                continue;
            }
            state.in_flight = true;
            let syncer = Arc::clone(&self.syncer);
            let entry = self.shared.entries()[index].clone();
            let handle = self
                .tasks
                .spawn(async move { Finished::Sync(syncer.sync(&entry).await) });
            self.running.insert(
                handle.id(),
                Running {
                    index,
                    started: now,
                },
            );
        }
    }

    fn finish(&mut self, joined: Result<(Id, Finished), JoinError>) {
        match joined {
            Ok((_, Finished::Ready(result))) => self.finish_ready_check(result),
            Ok((id, Finished::Sync(outcome))) => {
                if let Some(running) = self.running.remove(&id) {
                    self.record(running, outcome);
                }
            }
            // A panic in a sync task stops at the task boundary. It counts as
            // a failure of that entry only.
            Err(error) => match self.running.remove(&error.id()) {
                Some(running) => {
                    let repo = self.shared.entries()[running.index].repo_tag();
                    tracing::error!(repo, error = %error, "sync task failed");
                    let outcome =
                        SyncOutcome::error(ErrorKind::Internal, running.started.elapsed());
                    self.record(running, outcome);
                }
                None => {
                    tracing::error!(error = %error, "readiness check task failed");
                    self.finish_ready_check(Err(error.to_string()));
                }
            },
        }
    }

    fn finish_ready_check(&mut self, result: Result<(), String>) {
        self.ready_check_in_flight = false;
        match result {
            Ok(()) => {
                self.ready = true;
                self.health.set_ready(true);
                tracing::info!("startup checks passed; starting syncs");
            }
            Err(reason) => {
                self.ready_failures = self.ready_failures.saturating_add(1);
                let delay = exponential(READY_RETRY_INITIAL, self.ready_failures, READY_RETRY_MAX);
                self.next_ready_attempt = Instant::now() + delay;
                tracing::warn!(
                    reason,
                    retry_in_seconds = delay.as_secs(),
                    "Forgejo is not reachable with the configured token; not ready"
                );
            }
        }
    }

    fn record(&mut self, running: Running, outcome: SyncOutcome) {
        let Running { index, started } = running;
        let entry = &self.shared.entries()[index];
        self.metrics.sync_finished(entry, &outcome);

        let now = Instant::now();
        let poll_interval = self.config.poll_interval;
        let state = &mut self.states[index];
        state.in_flight = false;
        if outcome.result.is_success() {
            state.status = EntryStatus {
                last_success: Some(now),
                consecutive_failures: 0,
            };
            state.next_attempt = started + poll_interval;
        } else {
            state.status.consecutive_failures = state.status.consecutive_failures.saturating_add(1);
            let delay = failure_delay(
                poll_interval,
                state.status.consecutive_failures,
                outcome.retry_after,
                (self.jitter)(),
            );
            state.next_attempt = now + delay;
        }
        self.shared.update(index, state.status);
    }

    /// Shutdown: start nothing new, give in-flight syncs `shutdown_grace`,
    /// then cancel them and give their git children `cancel_grace` to die.
    async fn drain(mut self, ticker: &mut tokio::time::Interval, cancel_syncs: CancellationToken) {
        if self.tasks.is_empty() {
            return;
        }
        tracing::info!(
            in_flight = self.running.len(),
            "shutdown requested; waiting for in-flight syncs"
        );
        let grace = tokio::time::sleep(self.config.shutdown_grace);
        tokio::pin!(grace);
        loop {
            tokio::select! {
                () = &mut grace => break,
                _ = ticker.tick() => self.health.beat(),
                joined = self.tasks.join_next_with_id() => match joined {
                    Some(joined) => self.finish(joined),
                    None => return,
                },
            }
        }

        tracing::warn!(
            in_flight = self.running.len(),
            "in-flight syncs outlived the shutdown grace period; cancelling them"
        );
        cancel_syncs.cancel();
        // Outcomes of cancelled syncs are not recorded: the interruption is
        // ferry's own doing, and the next run repairs the repository.
        let stopped = tokio::time::timeout(self.config.cancel_grace, async {
            while self.tasks.join_next().await.is_some() {}
        })
        .await;
        if stopped.is_err() {
            self.tasks.shutdown().await;
        }
    }
}

/// `base × 2^(failures − 1)`, capped at `max`.
fn exponential(base: Duration, failures: u32, max: Duration) -> Duration {
    let doublings = failures.saturating_sub(1).min(31);
    base.saturating_mul(1u32 << doublings).min(max)
}

/// Delay before the next attempt after a failure: exponential backoff from
/// the poll interval, capped at `MAX_BACKOFF`, with ±10 % jitter. A forge
/// that named a longer wait (`retry_after`) wins.
///
/// `jitter` is in `-1.0..=1.0`.
pub fn failure_delay(
    poll_interval: Duration,
    consecutive_failures: u32,
    retry_after: Option<Duration>,
    jitter: f64,
) -> Duration {
    // A poll interval above the cap must not make failures retry sooner than
    // successes do.
    let cap = MAX_BACKOFF.max(poll_interval);
    let backoff = exponential(poll_interval, consecutive_failures, cap);
    let factor = 1.0 + JITTER_FRACTION * jitter.clamp(-1.0, 1.0);
    let jittered = backoff.mul_f64(factor);
    retry_after.map_or(jittered, |retry_after| jittered.max(retry_after))
}

/// Runs every entry once with bounded concurrency and no backoff. Results
/// come back in allowlist order.
pub async fn run_once(
    syncer: Arc<dyn Syncer>,
    metrics: &dyn Metrics,
    entries: &[RepoEntry],
    max_concurrency: usize,
) -> Vec<SyncOutcome> {
    let mut outcomes: Vec<Option<SyncOutcome>> = vec![None; entries.len()];
    let mut tasks: JoinSet<SyncOutcome> = JoinSet::new();
    let mut running: HashMap<Id, Running> = HashMap::new();
    let mut next = 0;

    loop {
        while next < entries.len() && tasks.len() < max_concurrency.max(1) {
            let syncer = Arc::clone(&syncer);
            let entry = entries[next].clone();
            let handle = tasks.spawn(async move { syncer.sync(&entry).await });
            running.insert(
                handle.id(),
                Running {
                    index: next,
                    started: Instant::now(),
                },
            );
            next += 1;
        }
        let Some(joined) = tasks.join_next_with_id().await else {
            break;
        };
        let (id, outcome) = match joined {
            Ok((id, outcome)) => (id, Some(outcome)),
            Err(error) => {
                tracing::error!(error = %error, "sync task failed");
                (error.id(), None)
            }
        };
        if let Some(Running { index, started }) = running.remove(&id) {
            let outcome = outcome
                .unwrap_or_else(|| SyncOutcome::error(ErrorKind::Internal, started.elapsed()));
            metrics.sync_finished(&entries[index], &outcome);
            outcomes[index] = Some(outcome);
        }
    }

    outcomes
        .into_iter()
        .map(|outcome| {
            outcome.unwrap_or_else(|| SyncOutcome::error(ErrorKind::Internal, Duration::ZERO))
        })
        .collect()
}

/// Settings of the periodic metrics emitter.
#[derive(Debug, Clone)]
pub struct EmitterConfig {
    pub interval: Duration,
    /// Directory measured for `ferry.cache.bytes`. `None` disables the gauge.
    pub cache_dir: Option<PathBuf>,
    /// The cache is measured at most this often: walking it is not free.
    pub cache_scan_interval: Duration,
}

impl EmitterConfig {
    pub fn from_config(config: &Config) -> Self {
        Self {
            interval: Duration::from_secs(30),
            cache_dir: Some(config.sync.cache_dir.clone()),
            cache_scan_interval: Duration::from_secs(15 * 60),
        }
    }
}

/// Emits the gauges every `interval` until `shutdown` is cancelled.
///
/// This task only reads scheduler state. It never writes the scheduler
/// heartbeat, and it emits `ferry.heartbeat` only while that heartbeat is
/// fresh, so a wedged scheduler shows up as missing data in Datadog.
pub async fn run_emitter(
    metrics: Arc<dyn Metrics>,
    health: HealthState,
    shared: SharedStatus,
    config: EmitterConfig,
    shutdown: CancellationToken,
) {
    let mut ticker = tokio::time::interval(config.interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_cache_scan: Option<Instant> = None;

    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            _ = ticker.tick() => {}
        }

        let now = Instant::now();
        metrics.repos_configured(shared.entries().len());
        for (entry, status) in shared.entries().iter().zip(shared.snapshot()) {
            metrics.repo_state(
                entry,
                shared.last_success_age(&status, now),
                status.consecutive_failures,
            );
        }
        if health.is_live() {
            metrics.heartbeat();
        }

        let scan_due = last_cache_scan
            .is_none_or(|last| now.saturating_duration_since(last) >= config.cache_scan_interval);
        if let Some(cache_dir) = config.cache_dir.as_ref().filter(|_| scan_due) {
            last_cache_scan = Some(now);
            let cache_dir = cache_dir.clone();
            match tokio::task::spawn_blocking(move || directory_bytes(&cache_dir)).await {
                Ok(bytes) => metrics.cache_bytes(bytes),
                Err(error) => tracing::warn!(error = %error, "cache size scan failed"),
            }
        }
    }
}

/// Sum of the file sizes under `path`. Unreadable entries count as zero:
/// git rewrites the cache while this walks it.
fn directory_bytes(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => directory_bytes(&entry.path()),
            Ok(kind) if kind.is_file() => entry.metadata().map_or(0, |meta| meta.len()),
            _ => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLL: Duration = Duration::from_secs(300);

    #[test]
    fn failure_delay_doubles_from_the_poll_interval() {
        assert_eq!(failure_delay(POLL, 1, None, 0.0), Duration::from_secs(300));
        assert_eq!(failure_delay(POLL, 2, None, 0.0), Duration::from_secs(600));
        assert_eq!(failure_delay(POLL, 3, None, 0.0), Duration::from_secs(1200));
        assert_eq!(failure_delay(POLL, 4, None, 0.0), Duration::from_secs(2400));
    }

    #[test]
    fn failure_delay_is_capped() {
        assert_eq!(failure_delay(POLL, 5, None, 0.0), MAX_BACKOFF);
        assert_eq!(failure_delay(POLL, u32::MAX, None, 0.0), MAX_BACKOFF);
        // The cap never undercuts a poll interval that is longer than it.
        let long_poll = Duration::from_secs(7200);
        assert_eq!(failure_delay(long_poll, 1, None, 0.0), long_poll);
        assert_eq!(failure_delay(long_poll, 9, None, 0.0), long_poll);
    }

    #[test]
    fn failure_delay_jitter_is_ten_percent() {
        assert_eq!(failure_delay(POLL, 1, None, 1.0), Duration::from_secs(330));
        assert_eq!(failure_delay(POLL, 1, None, -1.0), Duration::from_secs(270));
        // Out-of-range jitter is clamped.
        assert_eq!(failure_delay(POLL, 1, None, 50.0), Duration::from_secs(330));
    }

    #[test]
    fn failure_delay_honours_a_longer_retry_after() {
        let long = Some(Duration::from_secs(900));
        let short = Some(Duration::from_secs(10));
        assert_eq!(failure_delay(POLL, 1, long, 0.0), Duration::from_secs(900));
        assert_eq!(failure_delay(POLL, 1, short, 0.0), Duration::from_secs(300));
    }

    #[test]
    fn directory_bytes_sums_nested_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("a/b")).expect("mkdir");
        std::fs::write(dir.path().join("top"), [0u8; 10]).expect("write");
        std::fs::write(dir.path().join("a/b/nested"), [0u8; 32]).expect("write");
        assert_eq!(directory_bytes(dir.path()), 42);
        assert_eq!(directory_bytes(&dir.path().join("absent")), 0);
    }
}
