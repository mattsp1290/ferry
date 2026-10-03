//! Shared fixtures for the sync-flow tests: real git over `file://` remotes,
//! and stateful fakes of the GitHub and Forgejo REST APIs.
//!
//! `World` owns a temp directory with two "forges":
//!
//! - `github/<owner>/<name>.git` are bare repositories the tests push into
//!   through a scratch work tree (`SourceRepo`).
//! - `forgejo/<owner>/<name>.git` are bare repositories the fake Forgejo API
//!   creates on `POST …/repos`, the way Forgejo itself would.
//!
//! Every API call and every git operation appends one line to `World::events`,
//! so a test can assert what ran and in which order.

#![allow(dead_code)]

pub mod capture;
pub mod forge;
pub mod git;
pub mod logging_git;

pub use forge::*;
pub use git::*;
pub use logging_git::*;

use std::path::PathBuf;
use std::process::Output;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use ferry::config::{
    Config, ForgejoConfig, GithubConfig, HealthConfig, RepoEntry, SyncConfig, Token, TokenFiles,
};
use ferry::forge::{ForgejoClient, GithubClient, http_client};
use ferry::git::{Git, RefMap, Remote};
use ferry::sync::{ErrorKind, SyncContext, SyncOutcome, SyncResult};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer};

/// Obviously fake. Tests assert it never shows up in telemetry.
pub const FORGEJO_TOKEN: &str = "test-forgejo-token-not-real";
pub const GITHUB_TOKEN: &str = "test-github-token-not-real";
/// Login the fake Forgejo reports for the token.
pub const FORGEJO_LOGIN: &str = "ferry";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn assert_synced(outcome: &SyncOutcome) {
    assert_eq!(outcome.result, SyncResult::Synced, "{outcome:?}");
    assert_eq!(outcome.error_kind, None, "{outcome:?}");
}

pub fn assert_error(outcome: &SyncOutcome, kind: ErrorKind) {
    assert_eq!(outcome.result, SyncResult::Error, "{outcome:?}");
    assert_eq!(outcome.error_kind, Some(kind), "{outcome:?}");
}

/// Case 21 applies to every test: ferry never sends a DELETE request.
pub async fn assert_no_delete(world: &World) {
    assert_eq!(world.requests_with_method("DELETE").await, 0);
}

/// Polls `done` every 10 ms of real time until it is true, or panics naming
/// `what` after `timeout`.
pub async fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + timeout;
    while !done() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Lossy UTF-8 stdout of a finished process.
pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// An ordered log shared by the API fakes and the git wrapper.
#[derive(Debug, Clone, Default)]
pub struct Events(Arc<Mutex<Vec<String>>>);

impl Events {
    pub fn push(&self, event: impl Into<String>) {
        lock(&self.0).push(event.into());
    }

    pub fn all(&self) -> Vec<String> {
        lock(&self.0).clone()
    }

    pub fn clear(&self) {
        lock(&self.0).clear();
    }

    /// Events starting with `prefix`, in order.
    pub fn matching(&self, prefix: &str) -> Vec<String> {
        self.all()
            .into_iter()
            .filter(|event| event.starts_with(prefix))
            .collect()
    }

    pub fn count(&self, prefix: &str) -> usize {
        self.matching(prefix).len()
    }

    /// Index of the first event starting with `prefix`.
    pub fn position(&self, prefix: &str) -> Option<usize> {
        self.all()
            .iter()
            .position(|event| event.starts_with(prefix))
    }
}

pub struct World {
    pub root: TempDir,
    pub forgejo: MockServer,
    pub github: MockServer,
    pub state: Arc<Mutex<ForgeState>>,
    pub events: Events,
    pub git: Arc<LoggingGit>,
    pub config: Config,
    pub token_files: TokenFiles,
    /// Cancels the git runner's children, as shutdown does.
    pub cancel: CancellationToken,
}

impl World {
    pub async fn new() -> Self {
        Self::with_git_timeout(Duration::from_secs(60)).await
    }

    pub async fn with_git_timeout(git_timeout: Duration) -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let state = Arc::new(Mutex::new(ForgeState::default()));
        let events = Events::default();

        let forgejo = MockServer::start().await;
        Mock::given(any())
            .respond_with(ForgejoApi {
                root: root.path().to_path_buf(),
                state: Arc::clone(&state),
                events: events.clone(),
            })
            .mount(&forgejo)
            .await;
        let github = MockServer::start().await;
        Mock::given(any())
            .respond_with(GithubApi {
                state: Arc::clone(&state),
                events: events.clone(),
            })
            .mount(&github)
            .await;

        let secrets = root.path().join("secrets");
        std::fs::create_dir_all(&secrets).expect("create secrets dir");
        std::fs::write(secrets.join("forgejo-token"), format!("{FORGEJO_TOKEN}\n"))
            .expect("write token");
        let token_files = TokenFiles {
            github: None,
            forgejo: Some(secrets.join("forgejo-token")),
        };

        let config = Config {
            sync: SyncConfig {
                cache_dir: root.path().join("cache"),
                git_timeout_seconds: git_timeout.as_secs().max(1),
                ..SyncConfig::default()
            },
            github: GithubConfig {
                api_url: github.uri(),
                git_url: file_url(&root.path().join("github")),
            },
            forgejo: ForgejoConfig {
                url: forgejo.uri(),
                username: FORGEJO_LOGIN.to_string(),
            },
            health: HealthConfig::default(),
            repos: Vec::new(),
        };

        let cancel = CancellationToken::new();
        let git = logging_git(&config, &token_files, &events, &cancel);

        Self {
            root,
            forgejo,
            github,
            state,
            events,
            git,
            config,
            token_files,
            cancel,
        }
    }

    /// Simulates a process restart: a new git runner with a fresh
    /// cancellation token. The repositories and API state stay as they are.
    pub fn restart(&mut self) {
        self.cancel = CancellationToken::new();
        self.git = logging_git(&self.config, &self.token_files, &self.events, &self.cancel);
    }

    pub fn state(&self) -> MutexGuard<'_, ForgeState> {
        lock(&self.state)
    }

    /// An allowlist entry `owner/<name>` → `ferry/<name>` with LFS off.
    pub fn entry(name: &str) -> RepoEntry {
        RepoEntry {
            github: format!("owner/{name}"),
            forgejo: format!("{FORGEJO_LOGIN}/{name}"),
            lfs: false,
            actions: false,
            adopt: false,
        }
    }

    /// Creates the GitHub-side repository of `entry` with one commit on
    /// `main`, and registers a description with the fake GitHub API.
    pub fn source(&self, entry: &RepoEntry) -> SourceRepo {
        let (owner, name) = entry.github_parts();
        let bare = self.empty_source(entry);
        let work = self.root.path().join("work").join(owner).join(name);
        std::fs::create_dir_all(&work).expect("create work dir");
        git(
            &work,
            &["-c", "init.defaultBranch=main", "init", "--quiet", "."],
        );
        git(&work, &["remote", "add", "origin", &file_url(&bare)]);
        let source = SourceRepo { bare, work };
        source.commit("README.md", "first\n");
        self.state().github.insert(
            entry.repo_tag(),
            GithubAnswer::Description(format!("{name} description")),
        );
        source
    }

    /// An empty GitHub-side repository: no commits, no branches.
    pub fn empty_source(&self, entry: &RepoEntry) -> PathBuf {
        let (owner, name) = entry.github_parts();
        let bare = self
            .root
            .path()
            .join("github")
            .join(owner)
            .join(format!("{name}.git"));
        init_bare(&bare);
        bare
    }

    /// The bare repository behind the Forgejo side of `entry`.
    pub fn dest_git_dir(&self, entry: &RepoEntry) -> PathBuf {
        let (owner, name) = entry.forgejo_parts();
        forgejo_git_dir(self.root.path(), owner, name)
    }

    pub fn dest_refs(&self, entry: &RepoEntry) -> RefMap {
        ref_map(&self.dest_git_dir(entry))
    }

    /// Makes the Forgejo side of `entry` exist before ferry runs, as `repo`.
    pub fn existing_dest(&self, entry: &RepoEntry, repo: FakeRepo) -> PathBuf {
        let git_dir = self.dest_git_dir(entry);
        init_forgejo_git_dir(&git_dir);
        self.state()
            .repos
            .insert(entry.forgejo.to_lowercase(), repo);
        git_dir
    }

    /// Pushes every branch and tag of `source` into the Forgejo side of
    /// `entry`: a destination that already has content.
    pub fn seed_dest(&self, entry: &RepoEntry, source: &SourceRepo) {
        let dest = file_url(&self.dest_git_dir(entry));
        git(
            &source.bare,
            &[
                "push",
                "--quiet",
                &dest,
                "refs/heads/*:refs/heads/*",
                "refs/tags/*:refs/tags/*",
            ],
        );
    }

    pub fn dest(&self, entry: &RepoEntry) -> Option<FakeRepo> {
        self.state()
            .repos
            .get(&entry.forgejo.to_lowercase())
            .cloned()
    }

    pub fn context(&self) -> SyncContext {
        let http = http_client().expect("http client");
        SyncContext::new(
            &self.config,
            Arc::clone(&self.git) as Arc<dyn Git>,
            GithubClient::new(http.clone(), &self.config.github.api_url, None),
            ForgejoClient::new(http, &self.config.forgejo.url, Token::new(FORGEJO_TOKEN)),
        )
        .with_forgejo_git_url(&file_url(&self.root.path().join("forgejo")))
    }

    pub fn syncer(&self) -> Arc<SyncContext> {
        Arc::new(self.context())
    }

    /// The value ferry records in the LFS marker of `entry` for `refs`.
    pub fn lfs_marker_value(&self, entry: &RepoEntry, refs: &RefMap) -> String {
        let dest = Remote {
            url: format!(
                "{}.git",
                file_url(&self.dest_git_dir(entry)).trim_end_matches(".git")
            ),
            side: ferry::git::Side::Forgejo,
        };
        ferry::sync::lfs_marker_value(refs, &dest)
    }

    /// The cache repository ferry uses for `entry`.
    pub fn cache_path(&self, entry: &RepoEntry) -> PathBuf {
        let (owner, name) = entry.github_parts();
        self.config
            .sync
            .cache_dir
            .join("repos")
            .join(owner)
            .join(format!("{name}.git"))
    }

    /// Requests the fake servers received with the given HTTP method.
    pub async fn requests_with_method(&self, method: &str) -> usize {
        let mut count = 0;
        for server in [&self.forgejo, &self.github] {
            count += server
                .received_requests()
                .await
                .unwrap_or_default()
                .iter()
                .filter(|request| request.method.as_str() == method)
                .count();
        }
        count
    }
}
