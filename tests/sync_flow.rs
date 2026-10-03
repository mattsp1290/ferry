//! End-to-end sync tests: real git over `file://` remotes, fake REST APIs.
//!
//! The numbered cases follow the test list of the sync-engine work package
//! in the implementation plan.

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
use ferry::sync::marker::MARKER_FILE;
use ferry::sync::{ErrorKind, SyncOutcome, SyncResult, sync_repo};
use ferry::telemetry::{NoopMetrics, RecordingMetrics};
use support::{FakeRepo, GithubAnswer, World};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

fn assert_synced(outcome: &SyncOutcome) {
    assert_eq!(outcome.result, SyncResult::Synced, "{outcome:?}");
    assert_eq!(outcome.error_kind, None, "{outcome:?}");
}

fn assert_error(outcome: &SyncOutcome, kind: ErrorKind) {
    assert_eq!(outcome.result, SyncResult::Error, "{outcome:?}");
    assert_eq!(outcome.error_kind, Some(kind), "{outcome:?}");
}

/// Case 21 applies to every test: ferry never sends a DELETE request.
async fn assert_no_delete(world: &World) {
    assert_eq!(world.requests_with_method("DELETE").await, 0);
}

fn git_writes(world: &World) -> usize {
    world.events.count("git:push") + world.events.count("git:lfs_push")
}

#[tokio::test]
async fn case_01_missing_destination_is_created_private_marked_and_synced() {
    let world = World::new().await;
    let entry = World::entry("alpha");
    let source = world.source(&entry);
    source.tag("v1");
    let ctx = world.context();

    let outcome = sync_repo(&ctx, &entry).await;

    assert_synced(&outcome);
    assert_eq!(outcome.refs_changed, 2);
    assert_eq!(outcome.refs_pruned, 0);
    assert_eq!(world.dest_refs(&entry), source.refs());
    let dest = world.dest(&entry).expect("destination created");
    assert!(dest.private);
    assert!(dest.has_marker());
    assert!(!dest.has_actions, "Actions must be disabled by default");
    assert_eq!(dest.description, "alpha description");
    assert_eq!(world.events.count("forgejo:POST /api/v1/user/repos"), 1);
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_01b_destination_under_an_organization_uses_the_org_endpoint() {
    let world = World::new().await;
    let mut entry = World::entry("alpha");
    entry.forgejo = "mirrors/alpha".to_string();
    let source = world.source(&entry);
    let ctx = world.context();

    assert_synced(&sync_repo(&ctx, &entry).await);

    assert_eq!(world.dest_refs(&entry), source.refs());
    assert_eq!(
        world
            .events
            .count("forgejo:POST /api/v1/orgs/mirrors/repos"),
        1
    );
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_02_actions_entry_is_created_with_actions_enabled() {
    let world = World::new().await;
    let mut entry = World::entry("alpha");
    entry.actions = true;
    world.source(&entry);
    let ctx = world.context();

    assert_synced(&sync_repo(&ctx, &entry).await);

    assert!(world.dest(&entry).expect("created").has_actions);
    assert_eq!(
        world
            .events
            .matching("forgejo:PATCH")
            .iter()
            .filter(|event| event.contains("\"has_actions\":true"))
            .count(),
        1
    );
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_03_no_change_is_a_noop_without_fetch_or_push() {
    let world = World::new().await;
    let entry = World::entry("alpha");
    world.source(&entry);
    let ctx = world.context();
    assert_synced(&sync_repo(&ctx, &entry).await);
    world.events.clear();

    let outcome = sync_repo(&ctx, &entry).await;

    assert_eq!(outcome.result, SyncResult::Noop, "{outcome:?}");
    assert_eq!(outcome.refs_changed, 0);
    assert_eq!(world.events.count("git:fetch"), 0);
    assert_eq!(git_writes(&world), 0);
    assert_eq!(world.events.count("forgejo:PATCH"), 0);
    assert_eq!(world.events.count("forgejo:PUT"), 0);
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_04_new_commit_and_new_tag_are_synced() {
    let world = World::new().await;
    let entry = World::entry("alpha");
    let source = world.source(&entry);
    let ctx = world.context();
    assert_synced(&sync_repo(&ctx, &entry).await);

    source.commit("README.md", "second\n");
    source.annotated_tag("v2");
    let outcome = sync_repo(&ctx, &entry).await;

    assert_synced(&outcome);
    assert_eq!(outcome.refs_changed, 2);
    assert_eq!(outcome.refs_pruned, 0);
    assert_eq!(world.dest_refs(&entry), source.refs());
    assert_eq!(world.events.count("git:push --prune"), 0);
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_05_source_force_push_overwrites_the_destination() {
    let world = World::new().await;
    let entry = World::entry("alpha");
    let source = world.source(&entry);
    source.commit("README.md", "second\n");
    let ctx = world.context();
    assert_synced(&sync_repo(&ctx, &entry).await);
    let before = world.dest_refs(&entry);

    let rewritten = source.rewrite_tip("README.md", "rewritten\n");
    let outcome = sync_repo(&ctx, &entry).await;

    assert_synced(&outcome);
    assert_eq!(outcome.refs_changed, 1);
    let after = world.dest_refs(&entry);
    assert_ne!(after, before);
    assert_eq!(after.get("refs/heads/main"), Some(rewritten.as_str()));
    assert_eq!(after, source.refs());
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_06_deleted_source_branch_and_tag_are_pruned() {
    let world = World::new().await;
    let entry = World::entry("alpha");
    let source = world.source(&entry);
    source.branch("feature");
    source.tag("v1");
    let ctx = world.context();
    assert_synced(&sync_repo(&ctx, &entry).await);
    assert_eq!(world.dest_refs(&entry).len(), 3);

    source.delete_branch("feature");
    let outcome = sync_repo(&ctx, &entry).await;
    assert_synced(&outcome);
    assert_eq!(outcome.refs_pruned, 1);
    assert_eq!(outcome.refs_changed, 1);
    assert_eq!(world.dest_refs(&entry), source.refs());

    source.delete_tag("v1");
    let outcome = sync_repo(&ctx, &entry).await;
    assert_synced(&outcome);
    assert_eq!(outcome.refs_pruned, 1);
    assert_eq!(world.dest_refs(&entry), source.refs());
    assert_eq!(world.dest_refs(&entry).len(), 1);
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_07_default_branch_rename_switches_the_default_before_pruning() {
    let world = World::new().await;
    let entry = World::entry("alpha");
    let source = world.source(&entry);
    let ctx = world.context();
    assert_synced(&sync_repo(&ctx, &entry).await);
    assert_eq!(world.dest(&entry).expect("dest").default_branch, "main");

    source.rename_default_branch("main", "trunk");
    world.events.clear();
    let outcome = sync_repo(&ctx, &entry).await;

    // The fake Forgejo refuses to delete its default branch, as the real one
    // does, so a wrong order fails the prune push.
    assert_synced(&outcome);
    assert_eq!(outcome.refs_pruned, 1);
    assert_eq!(world.dest_refs(&entry), source.refs());
    assert_eq!(world.dest(&entry).expect("dest").default_branch, "trunk");

    let events = world.events.all();
    let position = |needle: &str| {
        events
            .iter()
            .position(|event| event.contains(needle))
            .unwrap_or_else(|| panic!("no event containing {needle:?} in {events:#?}"))
    };
    let first_push = events
        .iter()
        .position(|event| event == "git:push")
        .expect("first push");
    let edit = position("\"default_branch\":\"trunk\"");
    let prune = position("git:push --prune");
    assert!(first_push < edit && edit < prune, "{events:#?}");
    // The default branch comes from the HEAD symref of this pass, not from
    // the GitHub REST API.
    assert_eq!(world.events.count("github:"), 0, "{events:#?}");
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_08_source_without_branches_never_prunes_the_destination() {
    let world = World::new().await;
    let entry = World::entry("alpha");
    let source = world.source(&entry);
    source.tag("v1");
    let ctx = world.context();
    assert_synced(&sync_repo(&ctx, &entry).await);
    let before = world.dest_refs(&entry);

    source.delete_all_branches();
    world.events.clear();
    let outcome = sync_repo(&ctx, &entry).await;

    assert_error(&outcome, ErrorKind::SourceEmpty);
    assert_eq!(world.dest_refs(&entry), before);
    assert_eq!(git_writes(&world), 0);
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_08b_empty_source_and_empty_destination_is_empty() {
    let world = World::new().await;
    let entry = World::entry("alpha");
    world.empty_source(&entry);
    let ctx = world.context();

    let outcome = sync_repo(&ctx, &entry).await;
    assert_eq!(outcome.result, SyncResult::Empty, "{outcome:?}");
    assert!(world.dest(&entry).expect("created").has_marker());
    assert_eq!(git_writes(&world), 0);

    let outcome = sync_repo(&ctx, &entry).await;
    assert_eq!(outcome.result, SyncResult::Empty, "{outcome:?}");
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_09_pull_mirror_destination_is_refused() {
    let world = World::new().await;
    let entry = World::entry("alpha");
    world.source(&entry);
    world.existing_dest(
        &entry,
        FakeRepo {
            mirror: true,
            ..FakeRepo::marked()
        },
    );
    let ctx = world.context();

    let outcome = sync_repo(&ctx, &entry).await;

    assert_error(&outcome, ErrorKind::DestIsPullMirror);
    assert_eq!(git_writes(&world), 0);
    assert_eq!(world.events.count("git:fetch"), 0);
    assert!(world.dest_refs(&entry).is_empty());
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_10_missing_source_leaves_the_destination_alone() {
    let world = World::new().await;
    let entry = World::entry("alpha");
    let other = World::entry("other");
    let other_source = world.source(&other);
    world.existing_dest(&entry, FakeRepo::marked());
    world.seed_dest(&entry, &other_source);
    let before = world.dest_refs(&entry);
    let ctx = world.context();

    let outcome = sync_repo(&ctx, &entry).await;

    assert_error(&outcome, ErrorKind::SourceMissing);
    assert_eq!(world.dest_refs(&entry), before);
    assert_eq!(world.events.count("forgejo:"), 0);
    assert_eq!(git_writes(&world), 0);
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_11_lfs_failure_stops_before_any_ref_push() {
    let world = World::new().await;
    let mut entry = World::entry("alpha");
    entry.lfs = true;
    world.source(&entry);
    world.git.stub_lfs.store(true, Ordering::SeqCst);
    world.git.fail_lfs_push.store(true, Ordering::SeqCst);
    let ctx = world.context();

    let outcome = sync_repo(&ctx, &entry).await;

    assert_error(&outcome, ErrorKind::Lfs);
    assert_eq!(world.events.count("git:lfs_fetch"), 1);
    assert_eq!(world.events.count("git:lfs_push"), 1);
    assert_eq!(world.events.count("git:push"), 0);
    assert!(world.dest_refs(&entry).is_empty());
    assert!(!world.cache_path(&entry).join(MARKER_FILE).exists());

    // The next pass, with LFS working, converges.
    world.git.fail_lfs_push.store(false, Ordering::SeqCst);
    assert_synced(&sync_repo(&ctx, &entry).await);
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_12_unmarked_destination_with_content_is_refused() {
    let world = World::new().await;
    let entry = World::entry("alpha");
    world.source(&entry);
    let other = World::entry("other");
    let other_source = world.source(&other);
    world.existing_dest(&entry, FakeRepo::default());
    world.seed_dest(&entry, &other_source);
    let before = world.dest_refs(&entry);
    let ctx = world.context();

    let outcome = sync_repo(&ctx, &entry).await;

    assert_error(&outcome, ErrorKind::DestUnmanaged);
    assert_eq!(world.dest_refs(&entry), before);
    assert_eq!(git_writes(&world), 0);
    assert_eq!(world.events.count("git:fetch"), 0);
    assert_eq!(world.events.count("forgejo:PUT"), 0);
    assert!(!world.dest(&entry).expect("dest").has_marker());
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_13_adopt_marks_and_overwrites_an_unmarked_destination() {
    let world = World::new().await;
    let mut entry = World::entry("alpha");
    entry.adopt = true;
    let source = world.source(&entry);
    let other = World::entry("other");
    let other_source = world.source(&other);
    other_source.branch("stale");
    world.existing_dest(&entry, FakeRepo::default());
    world.seed_dest(&entry, &other_source);
    let ctx = world.context();

    let outcome = sync_repo(&ctx, &entry).await;

    assert_synced(&outcome);
    assert_eq!(outcome.refs_pruned, 1);
    assert_eq!(world.dest_refs(&entry), source.refs());
    let dest = world.dest(&entry).expect("dest");
    assert!(dest.has_marker());
    // An existing repository keeps its Actions setting and visibility.
    assert!(dest.has_actions);
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_14_empty_unmarked_destination_is_marked_and_synced() {
    let world = World::new().await;
    let entry = World::entry("alpha");
    let source = world.source(&entry);
    world.existing_dest(
        &entry,
        FakeRepo {
            private: false,
            ..FakeRepo::default()
        },
    );
    let ctx = world.context();

    let outcome = sync_repo(&ctx, &entry).await;

    assert_synced(&outcome);
    assert_eq!(world.dest_refs(&entry), source.refs());
    let dest = world.dest(&entry).expect("dest");
    assert!(dest.has_marker());
    assert!(!dest.private, "ferry must not change visibility");
    assert_eq!(world.events.count("forgejo:POST"), 0);
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_15_missing_lfs_marker_runs_the_lfs_steps_once() {
    let world = World::new().await;
    let mut entry = World::entry("alpha");
    let source = world.source(&entry);
    let ctx = world.context();
    assert_synced(&sync_repo(&ctx, &entry).await);
    assert_eq!(world.events.count("git:lfs_push"), 0);
    let marker = world.cache_path(&entry).join(MARKER_FILE);
    assert!(!marker.exists(), "lfs = false must not write a marker");

    // The entry is switched to lfs = true while the refs already match.
    entry.lfs = true;
    world.events.clear();
    let outcome = sync_repo(&ctx, &entry).await;

    assert_synced(&outcome);
    assert_eq!(outcome.refs_changed, 0);
    assert_eq!(world.events.count("git:lfs_fetch"), 1);
    assert_eq!(world.events.count("git:lfs_push"), 1);
    assert_eq!(
        std::fs::read_to_string(&marker).expect("marker").trim(),
        world.lfs_marker_value(&entry, &source.refs())
    );

    world.events.clear();
    let outcome = sync_repo(&ctx, &entry).await;
    assert_eq!(outcome.result, SyncResult::Noop, "{outcome:?}");
    assert_eq!(world.events.count("git:lfs_fetch"), 0);
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_16_github_rest_failure_does_not_fail_the_sync() {
    let mut world = World::new().await;
    // Every pass is due for a metadata refresh.
    world.config.sync.metadata_interval_seconds = 0;
    let entry = World::entry("alpha");
    let source = world.source(&entry);
    let ctx = world.context();
    assert_synced(&sync_repo(&ctx, &entry).await);

    world
        .state()
        .github
        .insert(entry.repo_tag(), GithubAnswer::Status(500));
    source.commit("README.md", "second\n");
    world.events.clear();
    let outcome = sync_repo(&ctx, &entry).await;

    assert_synced(&outcome);
    assert_eq!(world.events.count("github:GET"), 1);
    assert_eq!(world.dest_refs(&entry), source.refs());
    assert_eq!(
        world.dest(&entry).expect("dest").description,
        "alpha description"
    );
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_16b_description_follows_github_on_the_metadata_interval() {
    let mut world = World::new().await;
    world.config.sync.metadata_interval_seconds = 0;
    let entry = World::entry("alpha");
    world.source(&entry);
    let ctx = world.context();
    assert_synced(&sync_repo(&ctx, &entry).await);

    world.state().github.insert(
        entry.repo_tag(),
        GithubAnswer::Description("changed".to_string()),
    );
    let outcome = sync_repo(&ctx, &entry).await;

    assert_eq!(outcome.result, SyncResult::Noop, "{outcome:?}");
    assert_eq!(world.dest(&entry).expect("dest").description, "changed");
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_16c_github_rest_is_asked_at_most_once_per_metadata_interval() {
    let world = World::new().await;
    let entry = World::entry("alpha");
    let source = world.source(&entry);
    let ctx = world.context();
    assert_synced(&sync_repo(&ctx, &entry).await);
    source.commit("README.md", "second\n");
    assert_synced(&sync_repo(&ctx, &entry).await);
    assert_eq!(sync_repo(&ctx, &entry).await.result, SyncResult::Noop);

    assert_eq!(world.events.count("github:GET"), 1);
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_17_forgejo_edit_failure_with_equal_refs_is_a_metadata_error() {
    let world = World::new().await;
    let entry = World::entry("alpha");
    world.source(&entry);
    let ctx = world.context();
    assert_synced(&sync_repo(&ctx, &entry).await);

    {
        let mut state = world.state();
        state
            .repos
            .get_mut(&entry.forgejo)
            .expect("dest")
            .default_branch = "elsewhere".to_string();
        state.edit_status = Some(500);
    }
    let outcome = sync_repo(&ctx, &entry).await;
    assert_error(&outcome, ErrorKind::Metadata);

    world.state().edit_status = None;
    let outcome = sync_repo(&ctx, &entry).await;
    assert_eq!(outcome.result, SyncResult::Noop, "{outcome:?}");
    assert_eq!(world.dest(&entry).expect("dest").default_branch, "main");
    assert_no_delete(&world).await;
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
    let failure = SyncOutcome::error(ErrorKind::Network, Duration::ZERO);
    let syncer = ScriptedSyncer::new(Vec::new(), failure);
    let entries = [World::entry("alpha")];
    let shared = SharedStatus::new(&entries);
    let shutdown = CancellationToken::new();
    let scheduler = Scheduler::new(
        Arc::clone(&syncer) as Arc<dyn Syncer>,
        Arc::new(NoopMetrics),
        HealthState::new(),
        scheduler_config(Duration::from_secs(300), Duration::from_secs(10)),
        shared.clone(),
    )
    .with_jitter(|| 0.0);
    let task = tokio::spawn(scheduler.run(shutdown.clone(), CancellationToken::new()));
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
    let failure = SyncOutcome::error(ErrorKind::Network, Duration::ZERO);
    let success = SyncOutcome::success(SyncResult::Noop, Duration::ZERO);
    let syncer = ScriptedSyncer::new(vec![failure.clone(), failure], success);
    let entries = [World::entry("alpha")];
    let shared = SharedStatus::new(&entries);
    let shutdown = CancellationToken::new();
    let scheduler = Scheduler::new(
        Arc::clone(&syncer) as Arc<dyn Syncer>,
        Arc::new(NoopMetrics),
        HealthState::new(),
        scheduler_config(Duration::from_secs(300), Duration::from_secs(10)),
        shared.clone(),
    )
    .with_jitter(|| 0.0);
    let task = tokio::spawn(scheduler.run(shutdown.clone(), CancellationToken::new()));

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
    let shutdown = CancellationToken::new();
    let scheduler = Scheduler::new(
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
    );
    let task = tokio::spawn(scheduler.run(shutdown.clone(), world.cancel.clone()));

    // Wait until the sync is inside its (hanging) fetch.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while world.events.count("git:fetch") == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the sync never started"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

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
    let shutdown = CancellationToken::new();
    let scheduler = Scheduler::new(
        world.syncer(),
        Arc::clone(&metrics) as Arc<dyn ferry::telemetry::Metrics>,
        health.clone(),
        SchedulerConfig {
            poll_interval: Duration::from_millis(200),
            max_concurrency: 2,
            tick: Duration::from_millis(20),
            shutdown_grace: Duration::from_secs(10),
            cancel_grace: Duration::from_secs(5),
        },
        SharedStatus::new(&entries),
    );
    let task = tokio::spawn(scheduler.run(shutdown.clone(), world.cancel.clone()));

    let wait_for = |count: usize, repo: String| {
        let metrics = Arc::clone(&metrics);
        async move {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while metrics.outcomes_for(&repo).len() < count {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for {count} outcomes of {repo}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    };

    // Case 1 then case 3: created and synced, then nothing to do.
    wait_for(2, created.repo_tag()).await;
    // Case 6: a deleted branch is pruned on a later poll.
    source.delete_branch("feature");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !metrics
        .outcomes_for(&created.repo_tag())
        .iter()
        .any(|outcome| outcome.refs_pruned == 1)
    {
        assert!(std::time::Instant::now() < deadline, "the prune never ran");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    wait_for(1, missing.repo_tag()).await;
    wait_for(1, unmanaged.repo_tag()).await;
    shutdown.cancel();
    task.await.expect("scheduler exits");

    let outcomes = metrics.outcomes_for(&created.repo_tag());
    assert_eq!(outcomes[0].result, SyncResult::Synced);
    assert_eq!(outcomes[0].error_kind_tag(), "none");
    assert_eq!(outcomes[1].result, SyncResult::Noop);
    let pruned = outcomes
        .iter()
        .find(|outcome| outcome.refs_pruned == 1)
        .expect("prune outcome");
    assert_eq!(pruned.result, SyncResult::Synced);
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
    let shutdown = CancellationToken::new();
    let scheduler = Scheduler::new(
        world.syncer(),
        Arc::new(NoopMetrics),
        health.clone(),
        SchedulerConfig {
            poll_interval: Duration::from_millis(200),
            max_concurrency: 2,
            tick: Duration::from_millis(20),
            shutdown_grace: Duration::from_secs(10),
            cancel_grace: Duration::from_secs(5),
        },
        SharedStatus::new(&entries),
    );
    let task = tokio::spawn(scheduler.run(shutdown.clone(), world.cancel.clone()));

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

#[tokio::test]
async fn case_22_destination_changed_after_the_push_is_a_verify_mismatch() {
    let world = World::new().await;
    let mut entry = World::entry("alpha");
    entry.lfs = true;
    let source = world.source(&entry);
    world.git.stub_lfs.store(true, Ordering::SeqCst);
    let ctx = world.context();
    assert_synced(&sync_repo(&ctx, &entry).await);
    let marker = world.cache_path(&entry).join(MARKER_FILE);
    let old_marker = std::fs::read_to_string(&marker).expect("marker");
    let old_tip = source
        .refs()
        .get("refs/heads/main")
        .expect("main")
        .to_string();

    // Someone moves the branch back on Forgejo right after ferry's push.
    source.commit("README.md", "second\n");
    let dest = world.dest_git_dir(&entry);
    world.git.after_push.arm(move || {
        support::git(&dest, &["update-ref", "refs/heads/main", &old_tip]);
    });
    let outcome = sync_repo(&ctx, &entry).await;

    assert_error(&outcome, ErrorKind::VerifyMismatch);
    // An unverified sync must not record its LFS objects as complete.
    assert_eq!(
        std::fs::read_to_string(&marker).expect("marker"),
        old_marker
    );

    assert_synced(&sync_repo(&ctx, &entry).await);
    assert_eq!(world.dest_refs(&entry), source.refs());
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_08c_source_that_loses_its_branches_before_the_fetch_is_not_pruned() {
    let world = World::new().await;
    let entry = World::entry("alpha");
    let source = world.source(&entry);
    source.branch("feature");
    let ctx = world.context();
    assert_synced(&sync_repo(&ctx, &entry).await);
    let before = world.dest_refs(&entry);

    // The first ls-remote still sees branches; the fetch does not.
    source.commit("README.md", "second\n");
    let bare = source.bare.clone();
    world.git.before_fetch.arm(move || {
        let heads = support::git(
            &bare,
            &["for-each-ref", "--format=%(refname)", "refs/heads"],
        );
        for head in heads.lines() {
            support::git(&bare, &["update-ref", "-d", head]);
        }
    });
    world.events.clear();
    let outcome = sync_repo(&ctx, &entry).await;

    assert_error(&outcome, ErrorKind::SourceEmpty);
    assert_eq!(world.events.count("git:fetch"), 1);
    assert_eq!(git_writes(&world), 0);
    assert_eq!(world.dest_refs(&entry), before);
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_23_failed_provisioning_still_ends_with_actions_disabled() {
    let world = World::new().await;
    let entry = World::entry("alpha");
    let source = world.source(&entry);
    let ctx = world.context();

    // The repository is created, then the Actions edit fails.
    world.state().edit_status = Some(500);
    let outcome = sync_repo(&ctx, &entry).await;
    assert_eq!(outcome.result, SyncResult::Error, "{outcome:?}");
    let dest = world.dest(&entry).expect("created");
    assert!(
        dest.has_actions,
        "the fake creates repositories with Actions on"
    );
    assert_eq!(git_writes(&world), 0);

    // The next pass finds an existing repository and must finish the job.
    world.state().edit_status = None;
    assert_synced(&sync_repo(&ctx, &entry).await);
    let dest = world.dest(&entry).expect("dest");
    assert!(!dest.has_actions);
    assert!(dest.has_marker());
    assert_eq!(world.dest_refs(&entry), source.refs());
    assert_no_delete(&world).await;
}

#[tokio::test]
async fn case_24_unmarked_destination_forgejo_calls_non_empty_is_refused() {
    let world = World::new().await;
    let entry = World::entry("alpha");
    world.source(&entry);
    // No refs are listed, yet Forgejo says the repository has content. The
    // ref listing alone must not be enough to take a repository over.
    world.existing_dest(
        &entry,
        FakeRepo {
            reports_empty: Some(false),
            ..FakeRepo::default()
        },
    );
    let ctx = world.context();

    let outcome = sync_repo(&ctx, &entry).await;

    assert_error(&outcome, ErrorKind::DestUnmanaged);
    assert_eq!(world.events.count("forgejo:PUT"), 0);
    assert_eq!(git_writes(&world), 0);
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
        SyncOutcome::success(SyncResult::Noop, self.duration)
    }
}

#[tokio::test(start_paused = true)]
async fn concurrency_is_capped_and_the_longest_waiting_entry_goes_first() {
    // 8 entries, 100 s each, 2 at a time: one round takes 400 s, longer than
    // the 300 s poll interval. In plain config order the first entries would
    // become due again and the last ones would never run.
    let entries: Vec<RepoEntry> = (0..8).map(|n| World::entry(&format!("r{n}"))).collect();
    let syncer = ProbeSyncer::new(Duration::from_secs(100), None);
    let shutdown = CancellationToken::new();
    let scheduler = Scheduler::new(
        Arc::clone(&syncer) as Arc<dyn Syncer>,
        Arc::new(NoopMetrics),
        HealthState::new(),
        scheduler_config(Duration::from_secs(300), Duration::from_secs(10)),
        SharedStatus::new(&entries),
    );
    let task = tokio::spawn(scheduler.run(shutdown.clone(), CancellationToken::new()));

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
    let shutdown = CancellationToken::new();
    let scheduler = Scheduler::new(
        Arc::clone(&syncer) as Arc<dyn Syncer>,
        Arc::clone(&metrics) as Arc<dyn ferry::telemetry::Metrics>,
        HealthState::new(),
        scheduler_config(Duration::from_secs(300), Duration::from_secs(10)),
        shared.clone(),
    )
    .with_jitter(|| 0.0);
    let task = tokio::spawn(scheduler.run(shutdown.clone(), CancellationToken::new()));

    tokio::time::sleep(Duration::from_secs(305)).await;
    shutdown.cancel();
    task.await.expect("the scheduler survives a panicking sync");

    let bad = metrics.outcomes_for("owner/bad");
    assert_eq!(bad.len(), 2, "the entry is retried after its backoff");
    assert_error(&bad[0], ErrorKind::Internal);
    let good = metrics.outcomes_for("owner/good");
    assert_eq!(good.len(), 2);
    assert_eq!(good[0].result, SyncResult::Noop);
    assert_eq!(shared.snapshot()[0].consecutive_failures, 2);
    assert_eq!(shared.snapshot()[1].consecutive_failures, 0);
}

#[tokio::test(start_paused = true)]
async fn a_rate_limited_entry_waits_for_the_forge_but_not_forever() {
    let mut limited = SyncOutcome::error(ErrorKind::RateLimited, Duration::ZERO);
    limited.retry_after = Some(Duration::from_secs(1000));
    let mut hostile = SyncOutcome::error(ErrorKind::RateLimited, Duration::ZERO);
    hostile.retry_after = Some(Duration::MAX);
    let success = SyncOutcome::success(SyncResult::Noop, Duration::ZERO);
    let syncer = ScriptedSyncer::new(vec![limited, hostile], success);
    let entries = [World::entry("alpha")];
    let shutdown = CancellationToken::new();
    let scheduler = Scheduler::new(
        Arc::clone(&syncer) as Arc<dyn Syncer>,
        Arc::new(NoopMetrics),
        HealthState::new(),
        scheduler_config(Duration::from_secs(300), Duration::from_secs(10)),
        SharedStatus::new(&entries),
    )
    .with_jitter(|| 0.0);
    let task = tokio::spawn(scheduler.run(shutdown.clone(), CancellationToken::new()));

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
