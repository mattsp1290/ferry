//! Telemetry as the sync engine and scheduler produce it: traces and logs of
//! a real sync, the periodic gauges, and the rule that no credential reaches
//! any signal.

mod support;

use std::sync::Arc;
use std::time::Duration;

use ferry::config::RepoEntry;
use ferry::emitter::{EmitterConfig, run_emitter};
use ferry::health::HealthState;
use ferry::scheduler::{Scheduler, SchedulerConfig, SharedStatus, Syncer, run_once};
use ferry::sync::{ErrorKind, SyncOutcome, SyncStatus, sync_repo};
use ferry::telemetry::logging::dd_ids;
use ferry::telemetry::metrics::MetricEvent;
use ferry::telemetry::{self, Metrics, NoopMetrics, RecordingMetrics, Settings};
use opentelemetry::trace::{SpanId, Status};
use opentelemetry_sdk::trace::SpanData;
use support::capture::{Capture, attr_str, scoped_json_capture};
use support::{FORGEJO_TOKEN, GITHUB_TOKEN, World};
use tokio_util::sync::CancellationToken;

fn named<'a>(spans: &'a [SpanData], name: &str) -> Vec<&'a SpanData> {
    spans.iter().filter(|span| span.name == name).collect()
}

fn root(spans: &[SpanData]) -> &SpanData {
    let roots = named(spans, "ferry.sync_repo");
    assert_eq!(roots.len(), 1, "one ferry.sync_repo span per sync");
    roots[0]
}

#[tokio::test]
async fn one_sync_is_one_trace_with_a_span_for_each_step_that_ran() {
    let world = World::new().await;
    let entry = World::entry("alpha");
    world.source(&entry);
    let (_capture, exporter, _subscriber) = scoped_json_capture();

    let outcome = sync_repo(&world.context(), &entry).await;
    assert_eq!(outcome.result_tag(), "synced", "{outcome:?}");
    let spans = exporter.get_finished_spans().expect("spans");

    let root = root(&spans);
    assert_eq!(
        root.parent_span_id,
        SpanId::INVALID,
        "the sync span is a root"
    );
    assert_eq!(attr_str(root, "repo").as_deref(), Some("owner/alpha"));
    assert_eq!(
        attr_str(root, "forgejo_repo").as_deref(),
        Some("ferry/alpha")
    );
    assert_eq!(attr_str(root, "result").as_deref(), Some("synced"));
    assert_eq!(attr_str(root, "error_kind").as_deref(), Some("none"));
    assert_eq!(attr_str(root, "refs_changed").as_deref(), Some("1"));
    assert_eq!(
        attr_str(root, "operation.name").as_deref(),
        Some("ferry.sync_repo")
    );
    assert_eq!(root.status, Status::Unset);

    let trace_id = root.span_context.trace_id();
    let root_id = root.span_context.span_id();
    for span in &spans {
        assert_eq!(span.span_context.trace_id(), trace_id, "{}", span.name);
    }
    for name in [
        "git.ls_remote",
        "git.fetch",
        "git.push",
        "forgejo.api",
        "github.api",
    ] {
        let children = named(&spans, name);
        assert!(!children.is_empty(), "no {name} span");
        for child in children {
            assert_eq!(
                child.parent_span_id, root_id,
                "{name} is a child of the sync"
            );
        }
    }
    // LFS is off for this entry, so its steps did not run.
    assert!(named(&spans, "git.lfs_fetch").is_empty());
    assert!(named(&spans, "git.lfs_push").is_empty());

    let sides: Vec<_> = named(&spans, "git.ls_remote")
        .into_iter()
        .filter_map(|span| attr_str(span, "git.side"))
        .collect();
    assert!(sides.contains(&"github".to_string()), "{sides:?}");
    assert!(sides.contains(&"forgejo".to_string()), "{sides:?}");
    let push = named(&spans, "git.push")[0];
    assert_eq!(attr_str(push, "git.side").as_deref(), Some("forgejo"));
    assert_eq!(attr_str(push, "git.exit_code").as_deref(), Some("0"));

    for api in named(&spans, "forgejo.api") {
        let route = attr_str(api, "http.route").expect("route");
        assert!(route.starts_with("/api/v1/"), "{route}");
        // The route is the template, never the real path.
        assert!(!route.contains("alpha"), "{route}");
        assert!(attr_str(api, "http.method").is_some());
        assert!(attr_str(api, "http.status_code").is_some());
    }
}

#[tokio::test]
async fn an_error_outcome_marks_the_sync_span_as_an_error() {
    let world = World::new().await;
    let entry = World::entry("missing");
    let (capture, exporter, _subscriber) = scoped_json_capture();

    let outcome = sync_repo(&world.context(), &entry).await;
    assert!(matches!(
        outcome.status,
        SyncStatus::Failed {
            kind: ErrorKind::SourceMissing,
            ..
        }
    ));
    let spans = exporter.get_finished_spans().expect("spans");

    let root = root(&spans);
    assert!(
        matches!(root.status, Status::Error { .. }),
        "{:?}",
        root.status
    );
    assert_eq!(
        attr_str(root, "error.type").as_deref(),
        Some("source_missing")
    );
    assert_eq!(attr_str(root, "result").as_deref(), Some("error"));
    assert_eq!(
        attr_str(root, "error_kind").as_deref(),
        Some("source_missing")
    );

    let line = capture
        .json_lines()
        .into_iter()
        .find(|line| line["message"] == "sync failed")
        .expect("an error log line");
    assert_eq!(line["level"], "error");
    assert_eq!(line["error_kind"], "source_missing");
    assert_eq!(line["repo"], "owner/missing");
}

#[tokio::test]
async fn the_result_log_line_carries_the_trace_id_of_its_sync() {
    let world = World::new().await;
    let entry = World::entry("alpha");
    world.source(&entry);
    let (capture, exporter, _subscriber) = scoped_json_capture();

    let ctx = world.context();
    sync_repo(&ctx, &entry).await;
    let spans = exporter.get_finished_spans().expect("spans");
    let root = root(&spans);
    let (trace_id, span_id) = dd_ids(root.span_context.trace_id(), root.span_context.span_id());

    let lines = capture.json_lines();
    let finished = lines
        .iter()
        .find(|line| line["message"] == "sync finished")
        .expect("one line per finished sync");
    assert_eq!(finished["level"], "info");
    assert_eq!(finished["result"], "synced");
    assert_eq!(finished["error_kind"], "none");
    assert_eq!(finished["refs_changed"], 1);
    assert!(finished["duration_ms"].is_u64());
    assert_eq!(finished["repo"], "owner/alpha");
    assert_eq!(finished["forgejo_repo"], "ferry/alpha");
    assert_eq!(finished["service"], "ferry");
    assert_eq!(finished["dd.trace_id"], trace_id.as_str());
    assert_eq!(finished["dd.span_id"], span_id.as_str());

    // A pass that changes nothing logs below info, to limit volume.
    let before = capture.json_lines().len();
    let outcome = sync_repo(&ctx, &entry).await;
    assert_eq!(outcome.result_tag(), "noop");
    let noop: Vec<_> = capture.json_lines()[before..]
        .iter()
        .filter(|line| line["message"] == "sync finished")
        .cloned()
        .collect();
    assert_eq!(noop.len(), 1);
    assert_eq!(noop[0]["level"], "debug");
    assert_eq!(noop[0]["result"], "noop");
}

#[tokio::test]
async fn no_token_reaches_a_metric_a_log_line_or_a_span() {
    let world = World::new().await;
    let synced = World::entry("alpha");
    let missing = World::entry("missing");
    let unmanaged = World::entry("unmanaged");
    let source = world.source(&synced);
    world.source(&unmanaged);
    world.existing_dest(&unmanaged, support::FakeRepo::default());
    world.seed_dest(&unmanaged, &source);
    let (capture, exporter, _subscriber) = scoped_json_capture();
    let metrics = RecordingMetrics::new();

    let ctx = world.context();
    for entry in [&synced, &missing, &unmanaged] {
        let outcome = sync_repo(&ctx, entry).await;
        metrics.sync_finished(entry, &outcome);
    }

    let spans = format!("{:?}", exporter.get_finished_spans().expect("spans"));
    let logs = capture.text();
    let events = format!("{:?}", metrics.events());
    assert!(spans.contains("ferry.sync_repo") && logs.contains("sync finished"));
    for token in [FORGEJO_TOKEN, GITHUB_TOKEN] {
        assert!(!spans.contains(token), "token in a span");
        assert!(!logs.contains(token), "token in a log line");
        assert!(!events.contains(token), "token in a metric");
    }
    // Neither does a URL: span attributes carry route templates only.
    assert!(!spans.contains(&world.forgejo.uri()), "URL in a span");
}

/// Telemetry test 8: with both Datadog URLs unset, a pass still completes.
#[tokio::test]
async fn sync_completes_with_telemetry_disabled() {
    let world = World::new().await;
    let entry = World::entry("alpha");
    let source = world.source(&entry);
    let (guard, dispatch) = telemetry::build(Settings::default(), Capture::default());
    assert!(!guard.metrics_enabled());
    assert!(!guard.tracing_enabled());
    let _subscriber = tracing::dispatcher::set_default(&dispatch);

    let outcomes = run_once(
        world.syncer(),
        guard.metrics().as_ref(),
        std::slice::from_ref(&entry),
        2,
        &CancellationToken::new(),
    )
    .await;

    let outcome = outcomes[0].as_ref().expect("the entry ran");
    assert_eq!(outcome.result_tag(), "synced", "{outcome:?}");
    assert_eq!(world.dest_refs(&entry), source.refs());
}

// --- periodic gauges ---------------------------------------------------------

/// Fails `failures` times, then succeeds forever.
struct FlakySyncer {
    failures: std::sync::atomic::AtomicU32,
}

#[async_trait::async_trait]
impl Syncer for FlakySyncer {
    async fn check_ready(&self) -> Result<(), String> {
        Ok(())
    }

    async fn sync(&self, _entry: &RepoEntry) -> SyncOutcome {
        use std::sync::atomic::Ordering;
        let remaining = self.failures.load(Ordering::SeqCst);
        if remaining > 0 {
            self.failures.store(remaining - 1, Ordering::SeqCst);
            SyncOutcome {
                status: SyncStatus::Failed {
                    kind: ErrorKind::Network,
                    retry_after: None,
                },
                duration: Duration::ZERO,
            }
        } else {
            SyncOutcome {
                status: SyncStatus::Noop,
                duration: Duration::ZERO,
            }
        }
    }
}

fn repo_states(metrics: &RecordingMetrics) -> Vec<(Duration, u32)> {
    metrics
        .events()
        .into_iter()
        .filter_map(|event| match event {
            MetricEvent::RepoState {
                last_success_age,
                consecutive_failures,
                ..
            } => Some((last_success_age, consecutive_failures)),
            _ => None,
        })
        .collect()
}

fn emitter_config() -> EmitterConfig {
    EmitterConfig {
        interval: Duration::from_secs(30),
        cache_dir: None,
        cache_scan_interval: Duration::from_secs(900),
    }
}

/// Telemetry test 2: the age of the last success grows from process start
/// until the first success, then drops below the poll interval.
#[tokio::test(start_paused = true)]
async fn last_success_age_grows_until_the_first_success() {
    let poll_interval = Duration::from_secs(300);
    let entries = [World::entry("alpha")];
    let shared = SharedStatus::new(&entries);
    let health = HealthState::new();
    let metrics = Arc::new(RecordingMetrics::new());
    let shutdown = CancellationToken::new();

    let scheduler = Scheduler::new(
        Arc::new(FlakySyncer {
            failures: std::sync::atomic::AtomicU32::new(1),
        }),
        Arc::new(NoopMetrics),
        health.clone(),
        SchedulerConfig {
            poll_interval,
            max_concurrency: 1,
            tick: Duration::from_secs(10),
            shutdown_grace: Duration::from_secs(25),
            cancel_grace: Duration::from_secs(12),
        },
        shared.clone(),
    )
    .with_jitter(|| 0.0);
    let scheduler = tokio::spawn(scheduler.run(shutdown.clone(), CancellationToken::new()));
    let emitter = tokio::spawn(run_emitter(
        Arc::clone(&metrics) as Arc<dyn Metrics>,
        health,
        shared,
        emitter_config(),
        shutdown.clone(),
    ));

    // The only sync so far failed at once. The retry is due at 300 s.
    tokio::time::sleep(Duration::from_secs(295)).await;
    let before = repo_states(&metrics);
    let ages: Vec<u64> = before.iter().map(|(age, _)| age.as_secs()).collect();
    assert_eq!(ages, (0..=9).map(|n| n * 30).collect::<Vec<_>>());
    assert_eq!(before.last().expect("last").1, 1, "one failure so far");

    // The retry at 300 s succeeds.
    tokio::time::sleep(Duration::from_secs(40)).await;
    let (age, failures) = *repo_states(&metrics).last().expect("state");
    assert_eq!(age, Duration::from_secs(30));
    assert!(age < poll_interval);
    assert_eq!(failures, 0);

    shutdown.cancel();
    scheduler.await.expect("scheduler exits");
    emitter.await.expect("emitter exits");
}

/// Telemetry test 7: `ferry.heartbeat` stops when the scheduler loop does.
#[tokio::test(start_paused = true)]
async fn heartbeat_is_not_emitted_while_the_scheduler_heartbeat_is_stale() {
    let entries = [World::entry("alpha")];
    let health = HealthState::new();
    let metrics = Arc::new(RecordingMetrics::new());
    let shutdown = CancellationToken::new();
    let emitter = tokio::spawn(run_emitter(
        Arc::clone(&metrics) as Arc<dyn Metrics>,
        health.clone(),
        SharedStatus::new(&entries),
        emitter_config(),
        shutdown.clone(),
    ));

    // No scheduler tick has happened yet.
    tokio::time::sleep(Duration::from_secs(95)).await;
    assert_eq!(metrics.heartbeat_count(), 0);
    // The other gauges do not depend on the scheduler loop.
    assert!(
        metrics
            .events()
            .contains(&MetricEvent::ReposConfigured(entries.len()))
    );
    assert_eq!(repo_states(&metrics).len(), 4);

    // One tick: heartbeats at 120 s and 150 s, then the beat is 60 s old.
    health.beat();
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(metrics.heartbeat_count(), 2);

    shutdown.cancel();
    emitter.await.expect("emitter exits");
}

#[tokio::test]
async fn cache_size_is_measured_at_most_once_per_scan_interval() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("repos/owner")).expect("mkdir");
    std::fs::write(dir.path().join("repos/owner/pack"), [0u8; 1234]).expect("write");
    let metrics = Arc::new(RecordingMetrics::new());
    let shutdown = CancellationToken::new();
    let emitter = tokio::spawn(run_emitter(
        Arc::clone(&metrics) as Arc<dyn Metrics>,
        HealthState::new(),
        SharedStatus::new(&[]),
        EmitterConfig {
            interval: Duration::from_millis(10),
            cache_dir: Some(dir.path().to_path_buf()),
            cache_scan_interval: Duration::from_secs(3600),
        },
        shutdown.clone(),
    ));

    support::wait_until("the emitter to tick", Duration::from_secs(10), || {
        metrics
            .events()
            .iter()
            .filter(|event| matches!(event, MetricEvent::ReposConfigured(0)))
            .count()
            >= 5
    })
    .await;
    shutdown.cancel();
    emitter.await.expect("emitter exits");

    let scans: Vec<_> = metrics
        .events()
        .into_iter()
        .filter(|event| matches!(event, MetricEvent::CacheBytes(_)))
        .collect();
    assert_eq!(scans, [MetricEvent::CacheBytes(1234)]);
}
