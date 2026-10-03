//! End-to-end sync tests: real git over `file://` remotes, fake REST APIs.
//!
//! The numbered cases follow the test list of the sync-engine work package
//! in the implementation plan.

mod support;

use std::sync::atomic::Ordering;

use ferry::sync::marker::MARKER_FILE;
use ferry::sync::{ErrorKind, SyncResult, sync_repo};
use support::{FakeRepo, GithubAnswer, World, assert_error, assert_no_delete, assert_synced};

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
    world
        .git
        .before_fetch
        .arm(move || support::delete_all_branches(&bare));
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
