//! Polling loop: decides when each allowlist entry syncs.
//!
//! The scheduler is one `select!` loop. Syncs run on spawned tasks, so a long
//! sync never delays the tick, and the tick is the only writer of the
//! scheduler heartbeat. A wedged loop therefore stops the heartbeat and fails
//! liveness even while other tasks keep running.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use tokio::task::{Id, JoinError, JoinHandle, JoinSet};
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::config::{Config, RepoEntry};
use crate::health::HealthState;
use crate::sync::{ErrorKind, SyncOutcome};
use crate::telemetry::Metrics;
use crate::util::lock;

/// Upper bound of the failure backoff.
pub const MAX_BACKOFF: Duration = Duration::from_secs(3600);
/// Longest wait a forge may impose through `Retry-After`. The header is
/// remote input: unbounded, it could park an entry for years.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(6 * 3600);
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

    /// Applies `change` to one entry's status and returns the new value.
    fn update(&self, index: usize, change: impl FnOnce(&mut EntryStatus)) -> EntryStatus {
        let mut statuses = self.lock();
        change(&mut statuses[index]);
        statuses[index]
    }

    fn lock(&self) -> MutexGuard<'_, Vec<EntryStatus>> {
        lock(&self.statuses)
    }
}

struct EntryState {
    next_attempt: Instant,
    in_flight: bool,
}

struct Running {
    index: usize,
    started: Instant,
}

/// The running sync tasks and what each one is syncing.
struct SyncTasks {
    tasks: JoinSet<SyncOutcome>,
    running: HashMap<Id, Running>,
}

impl SyncTasks {
    fn new() -> Self {
        Self {
            tasks: JoinSet::new(),
            running: HashMap::new(),
        }
    }

    fn len(&self) -> usize {
        self.running.len()
    }

    fn is_empty(&self) -> bool {
        self.running.is_empty()
    }

    fn spawn(
        &mut self,
        syncer: &Arc<dyn Syncer>,
        index: usize,
        entry: RepoEntry,
        started: Instant,
    ) {
        let syncer = Arc::clone(syncer);
        let handle = self.tasks.spawn(async move { syncer.sync(&entry).await });
        self.running.insert(handle.id(), Running { index, started });
    }

    /// The next finished task and what it was syncing. A panic in a sync task
    /// stops at the task boundary and arrives as the `Err`.
    async fn join_next(&mut self) -> Option<(Running, Result<SyncOutcome, JoinError>)> {
        let joined = self.tasks.join_next_with_id().await?;
        let (id, result) = match joined {
            Ok((id, outcome)) => (id, Ok(outcome)),
            Err(error) => (error.id(), Err(error)),
        };
        // Every task is registered right after it is spawned, so the entry
        // is always there.
        let running = self.running.remove(&id)?;
        Some((running, result))
    }

    async fn shutdown(&mut self) {
        self.tasks.shutdown().await;
        self.running.clear();
    }
}

/// Progress of the startup check that needs the network.
enum Readiness {
    /// Not ready; the next check may start at `next_attempt`.
    Waiting {
        failures: u32,
        next_attempt: Instant,
    },
    Checking {
        failures: u32,
        check: JoinHandle<Result<(), String>>,
    },
    Ready,
}

impl Drop for Readiness {
    /// A dropped scheduler must not leave its readiness check running.
    fn drop(&mut self) {
        if let Self::Checking { check, .. } = self {
            check.abort();
        }
    }
}

/// Resolves when a running readiness check finishes; pending otherwise.
async fn readiness_check_finished(
    readiness: &mut Readiness,
) -> Result<Result<(), String>, JoinError> {
    match readiness {
        Readiness::Checking { check, .. } => check.await,
        _ => std::future::pending().await,
    }
}

pub struct Scheduler {
    syncer: Arc<dyn Syncer>,
    metrics: Arc<dyn Metrics>,
    health: HealthState,
    config: SchedulerConfig,
    shared: SharedStatus,
    states: Vec<EntryState>,
    syncs: SyncTasks,
    readiness: Readiness,
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
            })
            .collect();
        Self {
            syncer,
            metrics,
            health,
            config,
            shared,
            states,
            syncs: SyncTasks::new(),
            readiness: Readiness::Waiting {
                failures: 0,
                next_attempt: now,
            },
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
                Some((running, result)) = self.syncs.join_next() => {
                    self.finish_sync(running, result);
                    // Refill the freed slot now instead of at the next tick.
                    self.start_due();
                }
                result = readiness_check_finished(&mut self.readiness) => {
                    match result {
                        Ok(result) => self.finish_ready_check(result),
                        Err(error) => {
                            tracing::error!(error = %error, "readiness check task failed");
                            self.finish_ready_check(Err(error.to_string()));
                        }
                    }
                    self.start_due();
                }
            }
        }

        self.drain(&mut ticker, cancel_syncs).await;
    }

    fn start_ready_check(&mut self) {
        let Readiness::Waiting {
            failures,
            next_attempt,
        } = self.readiness
        else {
            return;
        };
        if Instant::now() < next_attempt {
            return;
        }
        let syncer = Arc::clone(&self.syncer);
        let check = tokio::spawn(async move { syncer.check_ready().await });
        self.readiness = Readiness::Checking { failures, check };
    }

    /// Starts due entries up to the concurrency limit, the longest-waiting
    /// first. Config order breaks ties. Serving entries in plain config order
    /// would starve the tail of an allowlist that is too large for one poll
    /// interval. No sync starts before the first successful readiness check.
    fn start_due(&mut self) {
        if !matches!(self.readiness, Readiness::Ready) {
            return;
        }
        let now = Instant::now();
        let free = self.config.max_concurrency.saturating_sub(self.syncs.len());
        let mut due: Vec<usize> = (0..self.states.len())
            .filter(|&index| {
                let state = &self.states[index];
                !state.in_flight && now >= state.next_attempt
            })
            .collect();
        due.sort_by_key(|&index| self.states[index].next_attempt);

        for index in due.into_iter().take(free) {
            self.states[index].in_flight = true;
            let entry = self.shared.entries()[index].clone();
            self.syncs.spawn(&self.syncer, index, entry, now);
        }
    }

    fn finish_sync(&mut self, running: Running, result: Result<SyncOutcome, JoinError>) {
        let outcome = match result {
            Ok(outcome) => outcome,
            // A panic in a sync task counts as a failure of that entry only.
            Err(error) => {
                let repo = self.shared.entries()[running.index].repo_tag();
                tracing::error!(repo, error = %error, "sync task failed");
                SyncOutcome::error(ErrorKind::Internal, running.started.elapsed())
            }
        };
        self.record(running, outcome);
    }

    fn finish_ready_check(&mut self, result: Result<(), String>) {
        let Readiness::Checking { failures, .. } = self.readiness else {
            return;
        };
        match result {
            Ok(()) => {
                self.readiness = Readiness::Ready;
                self.health.set_ready(true);
                tracing::info!("startup checks passed; starting syncs");
            }
            Err(reason) => {
                let failures = failures.saturating_add(1);
                let delay = exponential(READY_RETRY_INITIAL, failures, READY_RETRY_MAX);
                self.readiness = Readiness::Waiting {
                    failures,
                    next_attempt: later(Instant::now(), delay),
                };
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
            self.shared.update(index, |status| {
                *status = EntryStatus {
                    last_success: Some(now),
                    consecutive_failures: 0,
                };
            });
            state.next_attempt = later(started, poll_interval);
        } else {
            let status = self.shared.update(index, |status| {
                status.consecutive_failures = status.consecutive_failures.saturating_add(1);
            });
            let delay = failure_delay(
                poll_interval,
                status.consecutive_failures,
                outcome.retry_after,
                (self.jitter)(),
            );
            state.next_attempt = later(now, delay);
        }
    }

    /// Shutdown: start nothing new, give in-flight syncs `shutdown_grace`,
    /// then cancel them and give their git children `cancel_grace` to die.
    async fn drain(mut self, ticker: &mut tokio::time::Interval, cancel_syncs: CancellationToken) {
        if self.syncs.is_empty() {
            // At most a readiness check is in flight. Nothing waits for it.
            if let Readiness::Checking { check, .. } = &self.readiness {
                check.abort();
            }
            return;
        }
        tracing::info!(
            in_flight = self.syncs.len(),
            "shutdown requested; waiting for in-flight syncs"
        );
        let grace = tokio::time::sleep(self.config.shutdown_grace);
        tokio::pin!(grace);
        loop {
            tokio::select! {
                () = &mut grace => break,
                _ = ticker.tick() => self.health.beat(),
                joined = self.syncs.join_next() => match joined {
                    Some((running, result)) => self.finish_sync(running, result),
                    None => return,
                },
            }
        }

        tracing::warn!(
            in_flight = self.syncs.len(),
            "in-flight syncs outlived the shutdown grace period; cancelling them"
        );
        cancel_syncs.cancel();
        // Outcomes of cancelled syncs are not recorded: the interruption is
        // ferry's own doing, and the next run repairs the repository.
        let stopped = tokio::time::timeout(self.config.cancel_grace, async {
            while self.syncs.join_next().await.is_some() {}
        })
        .await;
        if stopped.is_err() {
            self.syncs.shutdown().await;
        }
    }
}

/// `from + delay`. `Instant` addition panics on overflow, and a panic here
/// would take the whole scheduler down, so an unrepresentable instant falls
/// back to the longest backoff.
fn later(from: Instant, delay: Duration) -> Instant {
    from.checked_add(delay).unwrap_or(from + MAX_BACKOFF)
}

/// `base × 2^(failures − 1)`, capped at `max`.
fn exponential(base: Duration, failures: u32, max: Duration) -> Duration {
    let doublings = failures.saturating_sub(1).min(31);
    base.saturating_mul(1u32 << doublings).min(max)
}

/// Delay before the next attempt after a failure: exponential backoff from
/// the poll interval, capped at `MAX_BACKOFF`, with ±10 % jitter. A forge
/// that named a longer wait (`retry_after`) wins, up to `MAX_RETRY_AFTER`.
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
    retry_after.map_or(jittered, |retry_after| {
        jittered.max(retry_after.min(MAX_RETRY_AFTER))
    })
}

/// Runs every entry once with bounded concurrency and no backoff. Results
/// come back in allowlist order.
///
/// Once `cancel` is cancelled no further entry starts. An entry that never
/// started has no outcome (`None`) and emits no metric.
pub async fn run_once(
    syncer: Arc<dyn Syncer>,
    metrics: &dyn Metrics,
    entries: &[RepoEntry],
    max_concurrency: usize,
    cancel: &CancellationToken,
) -> Vec<Option<SyncOutcome>> {
    let mut outcomes: Vec<Option<SyncOutcome>> = vec![None; entries.len()];
    let mut syncs = SyncTasks::new();
    let mut next = 0;

    loop {
        while next < entries.len() && syncs.len() < max_concurrency.max(1) && !cancel.is_cancelled()
        {
            syncs.spawn(&syncer, next, entries[next].clone(), Instant::now());
            next += 1;
        }
        let Some((Running { index, started }, result)) = syncs.join_next().await else {
            break;
        };
        let outcome = result.unwrap_or_else(|error| {
            tracing::error!(error = %error, "sync task failed");
            SyncOutcome::error(ErrorKind::Internal, started.elapsed())
        });
        metrics.sync_finished(&entries[index], &outcome);
        outcomes[index] = Some(outcome);
    }

    outcomes
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
    fn failure_delay_bounds_a_hostile_retry_after() {
        let delay = failure_delay(POLL, 1, Some(Duration::MAX), 0.0);
        assert_eq!(delay, MAX_RETRY_AFTER);
        // And the instant arithmetic on top of it cannot panic.
        let now = Instant::now();
        assert!(later(now, delay) > now);
        assert_eq!(later(now, Duration::MAX), now + MAX_BACKOFF);
    }
}
