//! The scheduler and `run_once`: ordering, backoff, concurrency, shutdown,
//! and the real scheduler driving real syncs. Scripted syncers use a paused
//! clock so start times are exact.

mod support;

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ferry::config::RepoEntry;
use ferry::forge::MARKER_TOPIC;
use ferry::health::HealthState;
use ferry::scheduler::{Scheduler, SchedulerConfig, SharedStatus, Syncer, run_once};
use ferry::sync::{ErrorKind, SyncOutcome, SyncStatus, sync_repo};
use ferry::telemetry::{Metrics, NoopMetrics, RecordingMetrics};
use support::{FakeRepo, World, assert_error, assert_no_delete, assert_synced};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Builds a scheduler with no failure jitter (so backoff is exact) and runs
/// it. Returns the shutdown token and the task.
fn spawn_scheduler(
    syncer: Arc<dyn Syncer>,
    metrics: Arc<dyn Metrics>,
    health: HealthState,
    config: SchedulerConfig,
    shared: SharedStatus,
    cancel_syncs: CancellationToken,
) -> (CancellationToken, JoinHandle<()>) {
    let shutdown = CancellationToken::new();
    let scheduler = Scheduler::new(syncer, metrics, health, config, shared).with_jitter(|| 0.0);
    let task = tokio::spawn(scheduler.run(shutdown.clone(), cancel_syncs));
    (shutdown, task)
}

/// A real-time config that polls fast enough for tests to finish quickly.
fn fast_scheduler_config() -> SchedulerConfig {
    SchedulerConfig {
        poll_interval: Duration::from_millis(200),
        max_concurrency: 2,
        tick: Duration::from_millis(20),
        shutdown_grace: Duration::from_secs(10),
        cancel_grace: Duration::from_secs(5),
    }
}

#[tokio::test]
async fn case_18_one_failing_entry_does_not_stop_the_next() {
    let world = World::new().await;
    let missing = World::entry("missing");
    let present = World::entry("present");
    let source = world.source(&present);
    let metrics = RecordingMetrics::new();

    let outcomes: Vec<SyncOutcome> = run_once(
        world.syncer(),
        &metrics,
        &[missing.clone(), present.clone()],
        1,
        &CancellationToken::new(),
    )
    .await
    .into_iter()
    .map(|outcome| outcome.expect("every entry ran"))
    .collect();

    assert_error(&outcomes[0], ErrorKind::SourceMissing);
    assert_synced(&outcomes[1]);
    assert_eq!(world.dest_refs(&present), source.refs());
    assert_eq!(
        metrics.outcomes_for(&missing.repo_tag()),
        [outcomes[0].clone()]
    );
    assert_eq!(
        metrics.outcomes_for(&present.repo_tag()),
        [outcomes[1].clone()]
    );
    assert_no_delete(&world).await;
}

/// A syncer that replays scripted outcomes and records when it was called.
struct ScriptedSyncer {
    started: Instant,
    script: Mutex<VecDeque<SyncOutcome>>,
    fallback: SyncOutcome,
    calls: Mutex<Vec<Duration>>,
}

impl ScriptedSyncer {
    fn new(script: Vec<SyncOutcome>, fallback: SyncOutcome) -> Arc<Self> {
        Arc::new(Self {
            started: Instant::now(),
            script: Mutex::new(script.into()),
            fallback,
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<Duration> {
        self.calls.lock().expect("calls").clone()
    }
}

#[async_trait]
impl Syncer for ScriptedSyncer {
    async fn check_ready(&self) -> Result<(), String> {
        Ok(())
    }

    async fn sync(&self, _entry: &RepoEntry) -> SyncOutcome {
        self.calls
            .lock()
            .expect("calls")
            .push(self.started.elapsed());
        self.script
            .lock()
            .expect("script")
            .pop_front()
            .unwrap_or_else(|| self.fallback.clone())
    }
}

fn scheduler_config(poll_interval: Duration, tick: Duration) -> SchedulerConfig {
    SchedulerConfig {
        poll_interval,
        max_concurrency: 2,
        tick,
        shutdown_grace: Duration::from_secs(25),
        cancel_grace: Duration::from_secs(12),
    }
}

#[tokio::test(start_paused = true)]
async fn case_19_failing_entry_is_not_retried_before_its_backoff() {
    let failure = SyncOutcome {
        status: SyncStatus::Failed {
            kind: ErrorKind::Network,
            retry_after: None,
        },
        duration: Duration::ZERO,
    };
    let syncer = ScriptedSyncer::new(Vec::new(), failure);
    let entries = [World::entry("alpha")];
    let shared = SharedStatus::new(&entries);
    let (shutdown, task) = spawn_scheduler(
        Arc::clone(&syncer) as Arc<dyn Syncer>,
        Arc::new(NoopMetrics),
        HealthState::new(),
        scheduler_config(Duration::from_secs(300), Duration::from_secs(10)),
        shared.clone(),
        CancellationToken::new(),
    );
    let seconds =
        |calls: Vec<Duration>| -> Vec<u64> { calls.iter().map(Duration::as_secs).collect() };

    // The first sync starts as soon as the readiness check passes.
    tokio::time::sleep(Duration::from_secs(295)).await;
    assert_eq!(seconds(syncer.calls()), [0]);
    // First failure: retry after one poll interval.
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(seconds(syncer.calls()), [0, 300]);
    // Second failure: the backoff doubles to 600 s.
    tokio::time::sleep(Duration::from_secs(590)).await;
    assert_eq!(seconds(syncer.calls()), [0, 300]);
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(seconds(syncer.calls()), [0, 300, 900]);
    assert_eq!(shared.snapshot()[0].consecutive_failures, 3);
    assert_eq!(shared.snapshot()[0].last_success, None);

    shutdown.cancel();
    task.await.expect("scheduler exits");
}

#[tokio::test(start_paused = true)]
async fn case_19b_success_resets_the_backoff_and_polls_on_the_interval() {
    let failure = SyncOutcome {
        status: SyncStatus::Failed {
            kind: ErrorKind::Network,
            retry_after: None,
        },
        duration: Duration::ZERO,
    };
    let success = SyncOutcome {
        status: SyncStatus::Noop,
        duration: Duration::ZERO,
    };
    let syncer = ScriptedSyncer::new(vec![failure.clone(), failure], success);
    let entries = [World::entry("alpha")];
    let shared = SharedStatus::new(&entries);
    let (shutdown, task) = spawn_scheduler(
        Arc::clone(&syncer) as Arc<dyn Syncer>,
        Arc::new(NoopMetrics),
        HealthState::new(),
        scheduler_config(Duration::from_secs(300), Duration::from_secs(10)),
        shared.clone(),
        CancellationToken::new(),
    );

    tokio::time::sleep(Duration::from_secs(1505)).await;
    let calls: Vec<u64> = syncer.calls().iter().map(Duration::as_secs).collect();
    // Fail at 0 and 300, succeed at 900, then every 300 s.
    assert_eq!(calls, [0, 300, 900, 1200, 1500]);
    let status = shared.snapshot()[0];
    assert_eq!(status.consecutive_failures, 0);
    assert!(status.last_success.is_some());

    shutdown.cancel();
    task.await.expect("scheduler exits");
}

#[tokio::test]
async fn case_20_shutdown_during_a_sync_exits_and_the_next_run_converges() {
    let mut world = World::new().await;
    let entry = World::entry("alpha");
    let source = world.source(&entry);
    world.git.hang_fetch.store(true, Ordering::SeqCst);

    let entries = [entry.clone()];
    let (shutdown, task) = spawn_scheduler(
        world.syncer(),
        Arc::new(NoopMetrics),
        HealthState::new(),
        SchedulerConfig {
            poll_interval: Duration::from_secs(300),
            max_concurrency: 2,
            tick: Duration::from_millis(20),
            shutdown_grace: Duration::from_millis(200),
            cancel_grace: Duration::from_secs(5),
        },
        SharedStatus::new(&entries),
        world.cancel.clone(),
    );

    // Wait until the sync is inside its (hanging) fetch.
    support::wait_until("the sync to start", Duration::from_secs(20), || {
        world.events.count("git:fetch") > 0
    })
    .await;

    let requested = std::time::Instant::now();
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the scheduler exits within the grace period")
        .expect("the scheduler does not panic");
    assert!(requested.elapsed() < Duration::from_secs(5));
    assert!(
        world.cancel.is_cancelled(),
        "in-flight syncs were cancelled"
    );
    assert_eq!(world.events.count("git:push"), 0);

    // A fresh process: the interrupted sync left nothing to clean up.
    world.restart();
    let ctx = world.context();
    assert_synced(&sync_repo(&ctx, &entry).await);
    assert_eq!(world.dest_refs(&entry), source.refs());
    assert_no_delete(&world).await;
}

/// Telemetry test 3 and integration gate 1: `RecordingMetrics` wired through
/// the real scheduler sees the result and error kind of each pass.
#[tokio::test]
async fn real_scheduler_records_results_and_error_kinds() {
    let world = World::new().await;
    let created = World::entry("created");
    let missing = World::entry("missing");
    let unmanaged = World::entry("unmanaged");
    let source = world.source(&created);
    source.branch("feature");
    world.source(&unmanaged);
    world.existing_dest(&unmanaged, FakeRepo::default());
    world.seed_dest(&unmanaged, &source);

    let entries = [created.clone(), missing.clone(), unmanaged.clone()];
    let metrics = Arc::new(RecordingMetrics::new());
    let health = HealthState::new();
    let (shutdown, task) = spawn_scheduler(
        world.syncer(),
        Arc::clone(&metrics) as Arc<dyn ferry::telemetry::Metrics>,
        health.clone(),
        fast_scheduler_config(),
        SharedStatus::new(&entries),
        world.cancel.clone(),
    );

    let wait_for = |count: usize, repo: String| {
        let metrics = Arc::clone(&metrics);
        async move {
            support::wait_until(
                &format!("{count} outcomes of {repo}"),
                Duration::from_secs(30),
                || metrics.outcomes_for(&repo).len() >= count,
            )
            .await;
        }
    };

    // Case 1 then case 3: created and synced, then nothing to do.
    wait_for(2, created.repo_tag()).await;
    // Case 6: a deleted branch is pruned on a later poll.
    source.delete_branch("feature");
    support::wait_until("the prune to run", Duration::from_secs(30), || {
        metrics
            .outcomes_for(&created.repo_tag())
            .iter()
            .any(|outcome| outcome.refs().1 == 1)
    })
    .await;
    wait_for(1, missing.repo_tag()).await;
    wait_for(1, unmanaged.repo_tag()).await;
    shutdown.cancel();
    task.await.expect("scheduler exits");

    let outcomes = metrics.outcomes_for(&created.repo_tag());
    assert_eq!(outcomes[0].result_tag(), "synced");
    assert_eq!(outcomes[0].error_kind_tag(), "none");
    assert_eq!(outcomes[1].result_tag(), "noop");
    let pruned = outcomes
        .iter()
        .find(|outcome| outcome.refs().1 == 1)
        .expect("prune outcome");
    assert_eq!(pruned.result_tag(), "synced");
    // Case 10 and case 12.
    assert_error(
        &metrics.outcomes_for(&missing.repo_tag())[0],
        ErrorKind::SourceMissing,
    );
    assert_error(
        &metrics.outcomes_for(&unmanaged.repo_tag())[0],
        ErrorKind::DestUnmanaged,
    );
    assert!(health.is_ready());
    assert!(health.is_live());
    assert!(
        world
            .dest(&unmanaged)
            .is_some_and(|dest| !dest.topics.iter().any(|topic| topic == MARKER_TOPIC))
    );
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn no_sync_starts_before_forgejo_accepts_the_token() {
    let world = World::new().await;
    let entry = World::entry("alpha");
    world.source(&entry);
    world.state().whoami_status = Some(503);

    let entries = [entry.clone()];
    let health = HealthState::new();
    let (shutdown, task) = spawn_scheduler(
        world.syncer(),
        Arc::new(NoopMetrics),
        health.clone(),
        fast_scheduler_config(),
        SharedStatus::new(&entries),
        world.cancel.clone(),
    );

    tokio::time::sleep(Duration::from_millis(400)).await;
    // Liveness does not depend on Forgejo; readiness does.
    assert!(health.is_live());
    assert!(!health.is_ready());
    assert_eq!(world.events.count("git:"), 0);
    assert!(world.events.count("forgejo:GET /api/v1/user") >= 1);

    shutdown.cancel();
    task.await.expect("scheduler exits");
    assert_no_delete(&world).await;
}

// --- scheduler behaviour with scripted syncers ---------------------------------

/// Records the order entries start in and how many run at once.
struct ProbeSyncer {
    started: Instant,
    duration: Duration,
    running: std::sync::atomic::AtomicUsize,
    peak: std::sync::atomic::AtomicUsize,
    starts: Mutex<Vec<(String, u64)>>,
    panic_on: Option<String>,
}

impl ProbeSyncer {
    fn new(duration: Duration, panic_on: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            started: Instant::now(),
            duration,
            running: std::sync::atomic::AtomicUsize::new(0),
            peak: std::sync::atomic::AtomicUsize::new(0),
            starts: Mutex::new(Vec::new()),
            panic_on: panic_on.map(str::to_string),
        })
    }

    fn starts(&self) -> Vec<(String, u64)> {
        self.starts.lock().expect("starts").clone()
    }
}

#[async_trait]
impl Syncer for ProbeSyncer {
    async fn check_ready(&self) -> Result<(), String> {
        Ok(())
    }

    async fn sync(&self, entry: &RepoEntry) -> SyncOutcome {
        self.starts
            .lock()
            .expect("starts")
            .push((entry.github.clone(), self.started.elapsed().as_secs()));
        assert!(
            self.panic_on.as_deref() != Some(entry.github.as_str()),
            "scripted panic"
        );
        let running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(running, Ordering::SeqCst);
        tokio::time::sleep(self.duration).await;
        self.running.fetch_sub(1, Ordering::SeqCst);
        SyncOutcome {
            status: SyncStatus::Noop,
            duration: self.duration,
        }
    }
}

#[tokio::test(start_paused = true)]
async fn concurrency_is_capped_and_the_longest_waiting_entry_goes_first() {
    // 8 entries, 100 s each, 2 at a time: one round takes 400 s, longer than
    // the 300 s poll interval. In plain config order the first entries would
    // become due again and the last ones would never run.
    let entries: Vec<RepoEntry> = (0..8).map(|n| World::entry(&format!("r{n}"))).collect();
    let syncer = ProbeSyncer::new(Duration::from_secs(100), None);
    let (shutdown, task) = spawn_scheduler(
        Arc::clone(&syncer) as Arc<dyn Syncer>,
        Arc::new(NoopMetrics),
        HealthState::new(),
        scheduler_config(Duration::from_secs(300), Duration::from_secs(10)),
        SharedStatus::new(&entries),
        CancellationToken::new(),
    );

    tokio::time::sleep(Duration::from_secs(795)).await;
    shutdown.cancel();
    task.await.expect("scheduler exits");

    assert_eq!(syncer.peak.load(Ordering::SeqCst), 2);
    let starts = syncer.starts();
    // A freed slot is refilled at once, not at the next tick.
    let first_round: Vec<u64> = starts.iter().take(8).map(|(_, at)| *at).collect();
    assert_eq!(first_round, [0, 0, 100, 100, 200, 200, 300, 300]);
    // Every entry ran in each of the first two rounds, in config order.
    for round in starts.chunks(8).take(2) {
        let names: Vec<&str> = round.iter().map(|(name, _)| name.as_str()).collect();
        let expected: Vec<String> = (0..8).map(|n| format!("owner/r{n}")).collect();
        assert_eq!(names, expected);
    }
}

#[tokio::test(start_paused = true)]
async fn a_panicking_sync_is_an_internal_error_for_that_entry_only() {
    let entries = [World::entry("bad"), World::entry("good")];
    let syncer = ProbeSyncer::new(Duration::from_secs(1), Some("owner/bad"));
    let metrics = Arc::new(RecordingMetrics::new());
    let shared = SharedStatus::new(&entries);
    let (shutdown, task) = spawn_scheduler(
        Arc::clone(&syncer) as Arc<dyn Syncer>,
        Arc::clone(&metrics) as Arc<dyn ferry::telemetry::Metrics>,
        HealthState::new(),
        scheduler_config(Duration::from_secs(300), Duration::from_secs(10)),
        shared.clone(),
        CancellationToken::new(),
    );

    tokio::time::sleep(Duration::from_secs(305)).await;
    shutdown.cancel();
    task.await.expect("the scheduler survives a panicking sync");

    let bad = metrics.outcomes_for("owner/bad");
    assert_eq!(bad.len(), 2, "the entry is retried after its backoff");
    assert_error(&bad[0], ErrorKind::Internal);
    let good = metrics.outcomes_for("owner/good");
    assert_eq!(good.len(), 2);
    assert_eq!(good[0].result_tag(), "noop");
    assert_eq!(shared.snapshot()[0].consecutive_failures, 2);
    assert_eq!(shared.snapshot()[1].consecutive_failures, 0);
}

#[tokio::test(start_paused = true)]
async fn a_rate_limited_entry_waits_for_the_forge_but_not_forever() {
    let limited = SyncOutcome {
        status: SyncStatus::Failed {
            kind: ErrorKind::RateLimited,
            retry_after: Some(Duration::from_secs(1000)),
        },
        duration: Duration::ZERO,
    };
    let hostile = SyncOutcome {
        status: SyncStatus::Failed {
            kind: ErrorKind::RateLimited,
            retry_after: Some(Duration::MAX),
        },
        duration: Duration::ZERO,
    };
    let success = SyncOutcome {
        status: SyncStatus::Noop,
        duration: Duration::ZERO,
    };
    let syncer = ScriptedSyncer::new(vec![limited, hostile], success);
    let entries = [World::entry("alpha")];
    let (shutdown, task) = spawn_scheduler(
        Arc::clone(&syncer) as Arc<dyn Syncer>,
        Arc::new(NoopMetrics),
        HealthState::new(),
        scheduler_config(Duration::from_secs(300), Duration::from_secs(10)),
        SharedStatus::new(&entries),
        CancellationToken::new(),
    );

    // Retry-After of 1000 s beats the 300 s backoff. An absurd value is
    // clamped to six hours instead of panicking or parking the entry.
    tokio::time::sleep(Duration::from_secs(1000 + 6 * 3600 + 5)).await;
    let calls: Vec<u64> = syncer.calls().iter().map(Duration::as_secs).collect();
    assert_eq!(calls[..3], [0, 1000, 1000 + 6 * 3600]);

    shutdown.cancel();
    task.await.expect("scheduler exits");
}

#[tokio::test]
async fn run_once_starts_nothing_after_cancellation() {
    let entries = [World::entry("alpha"), World::entry("beta")];
    let syncer = ProbeSyncer::new(Duration::from_millis(1), None);
    let metrics = RecordingMetrics::new();
    let cancel = CancellationToken::new();
    cancel.cancel();

    let outcomes = run_once(
        Arc::clone(&syncer) as Arc<dyn Syncer>,
        &metrics,
        &entries,
        2,
        &cancel,
    )
    .await;

    assert_eq!(outcomes, [None, None]);
    assert!(syncer.starts().is_empty());
    assert!(metrics.events().is_empty());
}
