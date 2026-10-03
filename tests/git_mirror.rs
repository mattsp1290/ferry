//! Integration tests for `GitRunner` against local bare repositories.
//!
//! LFS cases run only when `FERRY_TEST_LFS=1` is set.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ferry::git::{Git, GitErrorKind, GitRunner, GitSettings, Remote, Side};
use nix::sys::signal::kill;
use nix::unistd::Pid;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use support::{file_url, git, git_settings, write_script};

fn remote(path: &Path) -> Remote {
    Remote {
        url: file_url(path),
        side: Side::Github,
    }
}

fn settings(tmp: &Path, timeout: Duration, cancel: CancellationToken) -> GitSettings {
    GitSettings {
        github_host: "github.com".into(),
        forgejo_host: "forge.invalid".into(),
        forgejo_user: "ferry".into(),
        secrets: Vec::new(),
        ..git_settings(tmp.join("ferry-cache-dir"), timeout, cancel)
    }
}

struct Fixture {
    tmp: TempDir,
    src: PathBuf,
    work: PathBuf,
    dst: PathBuf,
    cache: PathBuf,
    runner: GitRunner,
}

impl Fixture {
    /// A source bare repository with branches `main` and `dev`, tag `v1`, and a
    /// `refs/pull/1/head`, plus an empty destination bare repository.
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let (src, work, dst, cache) = (
            root.join("src.git"),
            root.join("work"),
            root.join("dst.git"),
            root.join("cache.git"),
        );
        git(&root, &["init", "--bare", "-q", "-b", "main", "src.git"]);
        git(&root, &["init", "--bare", "-q", "-b", "main", "dst.git"]);
        git(&root, &["init", "-q", "-b", "main", "work"]);
        std::fs::write(work.join("a.txt"), "one\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "one"]);
        git(&work, &["branch", "dev"]);
        git(&work, &["tag", "v1"]);
        git(&work, &["remote", "add", "origin", src.to_str().unwrap()]);
        git(&work, &["push", "-q", "origin", "main", "dev", "v1"]);
        let head = git(&work, &["rev-parse", "HEAD"]);
        git(&src, &["update-ref", "refs/pull/1/head", &head]);
        let runner = GitRunner::new(settings(
            &root,
            Duration::from_secs(60),
            CancellationToken::new(),
        ));
        Self {
            tmp,
            src,
            work,
            dst,
            cache,
            runner,
        }
    }

    /// `git for-each-ref` of `heads` and `tags` in a bare repository.
    fn refs(&self, repo: &Path) -> String {
        git(
            repo,
            &[
                "for-each-ref",
                "--format=%(objectname) %(refname)",
                "refs/heads",
                "refs/tags",
            ],
        )
    }

    async fn sync(&self, prune: bool) {
        self.runner.ensure_cache(&self.cache).await.expect("cache");
        self.runner
            .fetch(&self.cache, &remote(&self.src))
            .await
            .expect("fetch");
        self.runner
            .push(&self.cache, &remote(&self.dst), prune)
            .await
            .expect("push");
    }
}

#[tokio::test]
async fn initial_mirror_copies_branches_and_tags() {
    let fx = Fixture::new();
    fx.sync(false).await;
    let expected = fx.refs(&fx.src);
    assert_eq!(expected.lines().count(), 3, "{expected}");
    assert_eq!(fx.refs(&fx.dst), expected);

    let local = fx.runner.local_refs(&fx.cache).await.expect("local refs");
    assert_eq!(local.len(), 3);
    assert!(local.has_heads());
    let remote_state = fx.runner.ls_remote(&remote(&fx.dst)).await.expect("ls");
    assert_eq!(remote_state.refs, local);
}

#[tokio::test]
async fn rewritten_source_branch_force_updates_destination() {
    let fx = Fixture::new();
    fx.sync(false).await;
    std::fs::write(fx.work.join("a.txt"), "rewritten\n").unwrap();
    git(
        &fx.work,
        &["commit", "-q", "--amend", "-a", "-m", "rewritten"],
    );
    git(&fx.work, &["push", "-q", "--force", "origin", "main"]);
    let new_head = git(&fx.work, &["rev-parse", "HEAD"]);

    fx.sync(false).await;
    assert_eq!(git(&fx.dst, &["rev-parse", "refs/heads/main"]), new_head);
    assert_eq!(fx.refs(&fx.dst), fx.refs(&fx.src));
}

#[tokio::test]
async fn prune_removes_deleted_branch_and_tag() {
    let fx = Fixture::new();
    fx.sync(false).await;
    git(&fx.src, &["update-ref", "-d", "refs/heads/dev"]);
    git(&fx.src, &["update-ref", "-d", "refs/tags/v1"]);

    // Without prune the destination keeps them.
    fx.sync(false).await;
    assert!(fx.refs(&fx.dst).contains("refs/heads/dev"));
    assert!(fx.refs(&fx.dst).contains("refs/tags/v1"));

    fx.sync(true).await;
    let dst_refs = fx.refs(&fx.dst);
    assert!(!dst_refs.contains("refs/heads/dev"), "{dst_refs}");
    assert!(!dst_refs.contains("refs/tags/v1"), "{dst_refs}");
    assert_eq!(dst_refs, fx.refs(&fx.src));
}

#[tokio::test]
async fn pull_refs_are_not_fetched_or_pushed() {
    let fx = Fixture::new();
    fx.sync(true).await;
    assert_eq!(git(&fx.cache, &["for-each-ref", "refs/pull"]), "");
    assert_eq!(git(&fx.dst, &["for-each-ref", "refs/pull"]), "");
    assert!(
        git(&fx.src, &["for-each-ref", "refs/pull"]).contains("refs/pull/1/head"),
        "fixture must have a pull ref"
    );
}

/// Writes an executable shell script that stands in for `git`.
fn fake_git(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    write_script(&path, body);
    path
}

async fn pid_is_gone(pid: i32) -> bool {
    for _ in 0..50 {
        if kill(Pid::from_raw(pid), None).is_err() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

async fn read_pid(path: &Path) -> i32 {
    for _ in 0..50 {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse()
        {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("script never wrote {}", path.display());
}

#[tokio::test]
async fn timeout_kills_the_whole_process_group() {
    let tmp = tempfile::tempdir().unwrap();
    let pidfile = tmp.path().join("child.pid");
    let script = fake_git(
        tmp.path(),
        "fake-git-timeout",
        &format!("sleep 300 &\necho $! > {}\nwait", pidfile.display()),
    );
    let mut s = settings(
        tmp.path(),
        Duration::from_millis(700),
        CancellationToken::new(),
    );
    s.git_program = script;
    let runner = GitRunner::new(s);

    let error = runner
        .ls_remote(&remote(tmp.path()))
        .await
        .expect_err("must time out");
    assert_eq!(error.kind, GitErrorKind::Timeout, "{error}");
    let pid = read_pid(&pidfile).await;
    assert!(pid_is_gone(pid).await, "grandchild {pid} survived");
}

#[tokio::test]
async fn timeout_escalates_to_sigkill_when_sigterm_is_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let pidfile = tmp.path().join("child.pid");
    let script = fake_git(
        tmp.path(),
        "fake-git-stubborn",
        &format!(
            "trap '' TERM\nsleep 300 &\necho $! > {}\nwhile true; do sleep 1; done",
            pidfile.display()
        ),
    );
    let mut s = settings(
        tmp.path(),
        Duration::from_millis(700),
        CancellationToken::new(),
    );
    s.git_program = script;
    let runner = GitRunner::new(s);

    let started = std::time::Instant::now();
    let error = runner
        .ls_remote(&remote(tmp.path()))
        .await
        .expect_err("must time out");
    assert_eq!(error.kind, GitErrorKind::Timeout, "{error}");
    assert!(started.elapsed() < Duration::from_secs(10));
    let pid = read_pid(&pidfile).await;
    assert!(pid_is_gone(pid).await, "grandchild {pid} survived SIGKILL");
}

#[tokio::test]
async fn cancellation_stops_the_child_with_cancelled_kind() {
    let tmp = tempfile::tempdir().unwrap();
    let pidfile = tmp.path().join("child.pid");
    let script = fake_git(
        tmp.path(),
        "fake-git-cancel",
        &format!("sleep 300 &\necho $! > {}\nwait", pidfile.display()),
    );
    let cancel = CancellationToken::new();
    let mut s = settings(tmp.path(), Duration::from_secs(120), cancel.clone());
    s.git_program = script;
    let runner = GitRunner::new(s);

    let trigger = tokio::spawn({
        let pidfile = pidfile.clone();
        async move {
            read_pid(&pidfile).await;
            cancel.cancel();
        }
    });
    let error = runner
        .ls_remote(&remote(tmp.path()))
        .await
        .expect_err("must be cancelled");
    trigger.await.unwrap();
    assert_eq!(error.kind, GitErrorKind::Cancelled, "{error}");
    let pid = read_pid(&pidfile).await;
    assert!(pid_is_gone(pid).await, "grandchild {pid} survived");

    // Once cancelled, no further child is spawned.
    let error = runner.ls_remote(&remote(tmp.path())).await.unwrap_err();
    assert_eq!(error.kind, GitErrorKind::Cancelled);
}

#[tokio::test]
async fn corrupt_cache_directory_is_reinitialised() {
    let fx = Fixture::new();
    fx.sync(false).await;

    // Corrupt it: the HEAD file is garbage and an object is junk.
    std::fs::write(fx.cache.join("HEAD"), "garbage\n").unwrap();
    std::fs::write(fx.cache.join("stray-file"), "junk").unwrap();
    assert!(fx.runner.local_refs(&fx.cache).await.is_err());

    fx.runner.ensure_cache(&fx.cache).await.expect("re-init");
    assert!(!fx.cache.join("stray-file").exists());
    assert!(fx.runner.local_refs(&fx.cache).await.unwrap().is_empty());

    // A healthy cache is left alone.
    fx.sync(false).await;
    std::fs::write(fx.cache.join("marker"), "keep").unwrap();
    fx.runner.ensure_cache(&fx.cache).await.unwrap();
    assert!(fx.cache.join("marker").exists());
}

#[tokio::test]
async fn invalid_directory_inside_a_repository_is_not_mistaken_for_the_parent() {
    let fx = Fixture::new();
    // `work` is a git work tree; a plain directory inside it is not a cache.
    let inner = fx.work.join("not-a-cache");
    std::fs::create_dir(&inner).unwrap();
    fx.runner.ensure_cache(&inner).await.expect("init");
    assert!(inner.join("HEAD").exists());
    assert!(inner.join("objects").exists());
}

#[tokio::test]
async fn ls_remote_reports_head_branch_and_none_for_empty_repository() {
    let fx = Fixture::new();
    let state = fx.runner.ls_remote(&remote(&fx.src)).await.expect("ls");
    assert_eq!(state.head.as_deref(), Some("main"));
    assert_eq!(state.refs.len(), 3, "pull refs excluded");
    assert!(state.refs.get("refs/heads/dev").is_some());
    assert!(state.refs.get("refs/pull/1/head").is_none());
    assert!(
        state.refs.iter().all(|(name, _)| !name.ends_with("^{}")),
        "peeled lines are dropped"
    );

    // Switch HEAD to another branch: ferry follows the symref.
    git(&fx.src, &["symbolic-ref", "HEAD", "refs/heads/dev"]);
    let state = fx.runner.ls_remote(&remote(&fx.src)).await.unwrap();
    assert_eq!(state.head.as_deref(), Some("dev"));

    let empty = fx
        .runner
        .ls_remote(&remote(&fx.dst))
        .await
        .expect("empty ok");
    assert!(empty.refs.is_empty());
    assert_eq!(empty.head, None);
}

#[tokio::test]
async fn annotated_tag_is_listed_once_without_peeled_line() {
    let fx = Fixture::new();
    git(&fx.work, &["tag", "-a", "-m", "release", "v2"]);
    git(&fx.work, &["push", "-q", "origin", "v2"]);
    let state = fx.runner.ls_remote(&remote(&fx.src)).await.unwrap();
    assert!(state.refs.get("refs/tags/v2").is_some());
    assert_eq!(state.refs.len(), 4);
}

#[tokio::test]
async fn missing_repository_is_classified_not_found() {
    let fx = Fixture::new();
    let error = fx
        .runner
        .ls_remote(&remote(&fx.tmp.path().join("nope.git")))
        .await
        .expect_err("must fail");
    assert_eq!(error.kind, GitErrorKind::NotFound, "{error}");
    assert_eq!(error.operation, "ls_remote");
    assert!(error.exit_code.is_some());
}

#[tokio::test]
async fn remote_urls_with_userinfo_are_refused_without_spawning() {
    let fx = Fixture::new();
    let bad = Remote {
        url: "https://user:secret@example.invalid/o/r.git".into(),
        side: Side::Forgejo,
    };
    let error = fx.runner.ls_remote(&bad).await.unwrap_err();
    assert_eq!(error.kind, GitErrorKind::Other);
    assert!(!error.to_string().contains("secret"));
}

#[tokio::test]
async fn runner_is_usable_as_a_trait_object_and_reports_versions() {
    let fx = Fixture::new();
    let git: Arc<dyn Git> = Arc::new(fx.runner.clone());
    git.ensure_cache(&fx.cache).await.unwrap();
    assert!(
        fx.runner
            .git_version()
            .await
            .unwrap()
            .starts_with("git version")
    );
}

// ---------------------------------------------------------------- LFS

fn lfs_enabled() -> bool {
    if std::env::var("FERRY_TEST_LFS").is_ok_and(|v| v == "1") {
        true
    } else {
        eprintln!("SKIP: FERRY_TEST_LFS not set");
        false
    }
}

/// Paths of every file under `<repo>/lfs/objects`.
fn lfs_objects(repo: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(&repo.join("lfs").join("objects"), &mut out);
    out
}

/// Adds an LFS-tracked file to the fixture's source repository.
fn add_lfs_file(fx: &Fixture) {
    git(&fx.work, &["lfs", "install", "--local"]);
    git(&fx.work, &["lfs", "track", "*.bin"]);
    std::fs::write(fx.work.join("big.bin"), "lfs payload, not real data\n").unwrap();
    git(&fx.work, &["add", "."]);
    git(&fx.work, &["commit", "-q", "-m", "add lfs file"]);
    // Pushing from the work tree uploads the object to the source store.
    git(&fx.work, &["push", "-q", "origin", "main"]);
}

#[tokio::test]
async fn lfs_objects_move_from_source_to_destination() {
    if !lfs_enabled() {
        return;
    }
    let fx = Fixture::new();
    assert!(
        fx.runner
            .lfs_version()
            .await
            .unwrap()
            .starts_with("git-lfs/")
    );
    add_lfs_file(&fx);
    assert_eq!(lfs_objects(&fx.src).len(), 1, "fixture stores the object");

    fx.runner.ensure_cache(&fx.cache).await.unwrap();
    fx.runner.fetch(&fx.cache, &remote(&fx.src)).await.unwrap();
    fx.runner
        .lfs_fetch(&fx.cache, &remote(&fx.src))
        .await
        .expect("lfs fetch");
    assert_eq!(lfs_objects(&fx.cache).len(), 1, "cache holds the object");

    assert!(lfs_objects(&fx.dst).is_empty());
    let dst = Remote {
        url: file_url(&fx.dst),
        side: Side::Forgejo,
    };
    fx.runner.lfs_push(&fx.cache, &dst).await.expect("lfs push");
    assert_eq!(
        lfs_objects(&fx.dst).len(),
        1,
        "destination holds the object"
    );
    fx.runner.push(&fx.cache, &dst, true).await.unwrap();

    // The cache repository stores no remote and no URL.
    let config = std::fs::read_to_string(fx.cache.join("config")).unwrap();
    assert!(!config.contains("[remote"), "{config}");
    assert!(!config.contains("url"), "{config}");
    assert!(!config.contains("file://"), "{config}");
}

/// A `.lfsconfig` committed to the mirrored repository must not choose where
/// ferry sends LFS objects.
#[tokio::test]
async fn lfsconfig_in_the_source_cannot_redirect_the_lfs_push() {
    if !lfs_enabled() {
        return;
    }
    let fx = Fixture::new();
    add_lfs_file(&fx);
    // After the object is stored: point LFS at another endpoint entirely.
    // Nothing listens there, so any use of it fails the command.
    std::fs::write(
        fx.work.join(".lfsconfig"),
        "[lfs]\n\turl = http://127.0.0.1:9/elsewhere/info/lfs\n",
    )
    .unwrap();
    git(&fx.work, &["add", ".lfsconfig"]);
    git(&fx.work, &["commit", "-q", "-m", "add .lfsconfig"]);
    git(
        &fx.work,
        &[
            "-c",
            "lfs.url=",
            "push",
            "-q",
            "--no-verify",
            "origin",
            "main",
        ],
    );

    fx.runner.ensure_cache(&fx.cache).await.unwrap();
    fx.runner.fetch(&fx.cache, &remote(&fx.src)).await.unwrap();
    fx.runner
        .lfs_fetch(&fx.cache, &remote(&fx.src))
        .await
        .expect("lfs fetch from the source, not from the .lfsconfig endpoint");
    assert_eq!(lfs_objects(&fx.cache).len(), 1, "cache holds the object");

    let dst = Remote {
        url: file_url(&fx.dst),
        side: Side::Forgejo,
    };
    fx.runner.lfs_push(&fx.cache, &dst).await.expect("lfs push");

    assert_eq!(
        lfs_objects(&fx.dst).len(),
        1,
        "the destination holds the object"
    );
}

/// A probe that never ran says nothing about the cache. Deleting the cache
/// then would throw away every fetched LFS object at shutdown.
#[tokio::test]
async fn cancelled_cache_probe_keeps_the_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let runner = GitRunner::new(settings(
        tmp.path(),
        Duration::from_secs(60),
        cancel.clone(),
    ));
    let cache = tmp.path().join("cache.git");
    runner.ensure_cache(&cache).await.unwrap();
    let object = cache.join("lfs-object-stand-in");
    std::fs::write(&object, "payload").unwrap();

    cancel.cancel();
    let error = runner.ensure_cache(&cache).await.unwrap_err();

    assert_eq!(error.kind, GitErrorKind::Cancelled, "{error}");
    assert!(object.exists(), "the cache was deleted");
}

/// `git` exiting 0 with unreadable output must be an error: for `ls-remote`,
/// empty output would otherwise read as "the remote has no refs".
#[tokio::test]
async fn output_over_the_cap_is_an_error_not_a_truncated_success() {
    let tmp = tempfile::tempdir().unwrap();
    // Prints more than the 64 MiB stdout cap, then exits 0.
    let script = fake_git(
        tmp.path(),
        "fake-git-flood",
        "head -c 70000000 /dev/zero | tr '\\0' 'x'",
    );
    let mut s = settings(
        tmp.path(),
        Duration::from_secs(120),
        CancellationToken::new(),
    );
    s.git_program = script;
    let runner = GitRunner::new(s);

    let error = runner
        .ls_remote(&remote(tmp.path()))
        .await
        .expect_err("truncated output must not be a success");

    assert_eq!(error.kind, GitErrorKind::Other, "{error}");
    assert_eq!(error.exit_code, Some(0));
}
