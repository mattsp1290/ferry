//! End-to-end sync tests: real git over `file://` remotes, fake REST APIs.
//!
//! The numbered cases follow the WP5 test list in
//! `.agents/plans/github-forgejo-sync-worker/03-sync-engine.md`.

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
        source.refs().hash()
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

    let outcomes = run_once(
        world.syncer(),
        &metrics,
        &[missing.clone(), present.clone()],
        1,
    )
    .await;

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

    // Tick 0 runs the readiness check, tick 1 starts the first sync.
    tokio::time::sleep(Duration::from_secs(305)).await;
    assert_eq!(seconds(syncer.calls()), [10]);
    // First failure: retry after one poll interval.
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(seconds(syncer.calls()), [10, 310]);
    // Second failure: the backoff doubles to 600 s.
    tokio::time::sleep(Duration::from_secs(590)).await;
    assert_eq!(seconds(syncer.calls()), [10, 310]);
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(seconds(syncer.calls()), [10, 310, 910]);
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

    tokio::time::sleep(Duration::from_secs(1525)).await;
    let calls: Vec<u64> = syncer.calls().iter().map(Duration::as_secs).collect();
    // Fail at 10 and 310, succeed at 910, then every 300 s.
    assert_eq!(calls, [10, 310, 910, 1210, 1510]);
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
