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

use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use ferry::config::{
    Config, ForgejoConfig, GithubConfig, HealthConfig, RepoEntry, SyncConfig, Token, TokenFiles,
};
use ferry::forge::{ForgejoClient, GithubClient, MARKER_TOPIC, http_client};
use ferry::git::{
    Git, GitError, GitErrorKind, GitRunner, GitSettings, RefMap, Remote, RemoteState,
};
use ferry::sync::{RepoSyncer, SyncContext};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Obviously fake. Tests assert it never shows up in telemetry.
pub const FORGEJO_TOKEN: &str = "test-forgejo-token-not-real";
pub const GITHUB_TOKEN: &str = "test-github-token-not-real";
/// Login the fake Forgejo reports for the token.
pub const FORGEJO_LOGIN: &str = "ferry";

/// Runs plain `git` hermetically and returns trimmed stdout.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

pub fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// `refs/heads/*` and `refs/tags/*` of a bare repository, as ferry sees them.
pub fn ref_map(bare: &Path) -> RefMap {
    let text = git(
        bare,
        &[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/heads",
            "refs/tags",
        ],
    );
    ferry::git::refs::parse_for_each_ref(&text)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
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

/// A repository as the fake Forgejo API reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeRepo {
    pub private: bool,
    pub mirror: bool,
    pub default_branch: String,
    pub description: String,
    pub has_actions: bool,
    pub topics: Vec<String>,
    /// What the API reports as `empty`. `None` derives it from the refs.
    pub reports_empty: Option<bool>,
}

impl Default for FakeRepo {
    fn default() -> Self {
        Self {
            private: true,
            mirror: false,
            default_branch: "main".to_string(),
            description: String::new(),
            // Forgejo enables the Actions unit on new repositories.
            has_actions: true,
            topics: Vec::new(),
            reports_empty: None,
        }
    }
}

impl FakeRepo {
    pub fn has_marker(&self) -> bool {
        self.topics.iter().any(|topic| topic == MARKER_TOPIC)
    }

    pub fn marked() -> Self {
        Self {
            topics: vec![MARKER_TOPIC.to_string()],
            ..Self::default()
        }
    }

    fn to_json(&self, git_dir: &Path) -> Value {
        json!({
            "private": self.private,
            "mirror": self.mirror,
            "default_branch": self.default_branch,
            "description": self.description,
            "has_actions": self.has_actions,
            "topics": self.topics,
            "empty": self
                .reports_empty
                .unwrap_or_else(|| ref_map(git_dir).is_empty()),
        })
    }
}

/// How the fake GitHub API answers `GET /repos/{owner}/{repo}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GithubAnswer {
    Description(String),
    Status(u16),
}

#[derive(Debug, Default)]
pub struct ForgeState {
    /// Forgejo repositories keyed by lowercase `owner/name`.
    pub repos: BTreeMap<String, FakeRepo>,
    /// GitHub REST answers keyed by lowercase `owner/name`. Missing → 404.
    pub github: HashMap<String, GithubAnswer>,
    /// When set, `PATCH …/repos/{owner}/{repo}` answers with this status.
    pub edit_status: Option<u16>,
    /// When set, `GET /api/v1/user` answers with this status.
    pub whoami_status: Option<u16>,
}

struct ForgejoApi {
    root: PathBuf,
    state: Arc<Mutex<ForgeState>>,
    events: Events,
}

impl ForgejoApi {
    fn git_dir(&self, owner: &str, name: &str) -> PathBuf {
        forgejo_git_dir(&self.root, owner, name)
    }

    fn create(&self, owner: &str, body: &Value) -> ResponseTemplate {
        let name = body["name"].as_str().unwrap_or_default().to_string();
        let key = format!("{owner}/{name}").to_lowercase();
        let mut state = lock(&self.state);
        if state.repos.contains_key(&key) {
            return ResponseTemplate::new(409);
        }
        let repo = FakeRepo {
            private: body["private"].as_bool().unwrap_or(false),
            description: body["description"].as_str().unwrap_or_default().to_string(),
            ..FakeRepo::default()
        };
        let git_dir = self.git_dir(owner, &name);
        init_forgejo_git_dir(&git_dir);
        let response = ResponseTemplate::new(201).set_body_json(repo.to_json(&git_dir));
        state.repos.insert(key, repo);
        response
    }

    fn edit(&self, owner: &str, name: &str, body: &Value) -> ResponseTemplate {
        let key = format!("{owner}/{name}").to_lowercase();
        let mut state = lock(&self.state);
        if let Some(status) = state.edit_status {
            return ResponseTemplate::new(status);
        }
        let git_dir = self.git_dir(owner, name);
        let Some(repo) = state.repos.get_mut(&key) else {
            return ResponseTemplate::new(404);
        };
        if let Some(branch) = body["default_branch"].as_str() {
            repo.default_branch = branch.to_string();
            git(
                &git_dir,
                &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")],
            );
        }
        if let Some(description) = body["description"].as_str() {
            repo.description = description.to_string();
        }
        if let Some(has_actions) = body["has_actions"].as_bool() {
            repo.has_actions = has_actions;
        }
        ResponseTemplate::new(200).set_body_json(repo.to_json(&git_dir))
    }
}

impl Respond for ForgejoApi {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let method = request.method.as_str().to_string();
        let path = request.url.path().to_string();
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let mut event = format!("forgejo:{method} {path}");
        if method == "PATCH" {
            event.push_str(&format!(" {body}"));
        }
        self.events.push(event);

        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        match (method.as_str(), segments.as_slice()) {
            ("GET", ["api", "v1", "user"]) => match lock(&self.state).whoami_status {
                Some(status) => ResponseTemplate::new(status),
                None => ResponseTemplate::new(200).set_body_json(json!({"login": FORGEJO_LOGIN})),
            },
            ("GET", ["api", "v1", "repos", owner, name]) => {
                let key = format!("{owner}/{name}").to_lowercase();
                match lock(&self.state).repos.get(&key) {
                    Some(repo) => ResponseTemplate::new(200)
                        .set_body_json(repo.to_json(&self.git_dir(owner, name))),
                    None => ResponseTemplate::new(404),
                }
            }
            ("POST", ["api", "v1", "user", "repos"]) => self.create(FORGEJO_LOGIN, &body),
            ("POST", ["api", "v1", "orgs", owner, "repos"]) => self.create(owner, &body),
            ("PATCH", ["api", "v1", "repos", owner, name]) => self.edit(owner, name, &body),
            ("GET", ["api", "v1", "repos", owner, name, "topics"]) => {
                let key = format!("{owner}/{name}").to_lowercase();
                match lock(&self.state).repos.get(&key) {
                    Some(repo) => {
                        ResponseTemplate::new(200).set_body_json(json!({"topics": repo.topics}))
                    }
                    None => ResponseTemplate::new(404),
                }
            }
            ("PUT", ["api", "v1", "repos", owner, name, "topics", topic]) => {
                let key = format!("{owner}/{name}").to_lowercase();
                match lock(&self.state).repos.get_mut(&key) {
                    Some(repo) => {
                        if !repo.topics.iter().any(|existing| existing == topic) {
                            repo.topics.push((*topic).to_string());
                        }
                        ResponseTemplate::new(204)
                    }
                    None => ResponseTemplate::new(404),
                }
            }
            _ => ResponseTemplate::new(404),
        }
    }
}

struct GithubApi {
    state: Arc<Mutex<ForgeState>>,
    events: Events,
}

impl Respond for GithubApi {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path().to_string();
        self.events
            .push(format!("github:{} {path}", request.method.as_str()));
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        let ["repos", owner, name] = segments.as_slice() else {
            return ResponseTemplate::new(404);
        };
        let key = format!("{owner}/{name}").to_lowercase();
        match lock(&self.state).github.get(&key) {
            Some(GithubAnswer::Description(description)) => ResponseTemplate::new(200)
                .set_body_json(json!({
                    "description": description,
                    "private": false,
                    "archived": false,
                })),
            Some(GithubAnswer::Status(status)) => ResponseTemplate::new(*status),
            None => ResponseTemplate::new(404),
        }
    }
}

fn forgejo_git_dir(root: &Path, owner: &str, name: &str) -> PathBuf {
    root.join("forgejo").join(owner).join(format!("{name}.git"))
}

/// Creates a bare repository that, like Forgejo, refuses to delete the
/// branch its `HEAD` points at.
fn init_forgejo_git_dir(git_dir: &Path) {
    std::fs::create_dir_all(git_dir).expect("create forgejo repo dir");
    git(
        git_dir,
        &[
            "-c",
            "init.defaultBranch=main",
            "init",
            "--bare",
            "--quiet",
            ".",
        ],
    );
    let hook = git_dir.join("hooks").join("pre-receive");
    std::fs::create_dir_all(hook.parent().expect("hooks dir")).expect("create hooks dir");
    std::fs::write(
        &hook,
        "#!/bin/sh\n\
         head=$(git symbolic-ref HEAD)\n\
         zero=0000000000000000000000000000000000000000\n\
         while read -r old new ref; do\n\
         \tif [ \"$new\" = \"$zero\" ] && [ \"$ref\" = \"$head\" ]; then\n\
         \t\techo \"refusing to delete the default branch $ref\" >&2\n\
         \t\texit 1\n\
         \tfi\n\
         done\n",
    )
    .expect("write hook");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod hook");
}

/// A GitHub-side repository: a bare repository plus a scratch work tree.
pub struct SourceRepo {
    pub bare: PathBuf,
    pub work: PathBuf,
}

impl SourceRepo {
    fn git(&self, args: &[&str]) -> String {
        git(&self.work, args)
    }

    /// Commits `content` to `file` on the current branch and pushes it.
    pub fn commit(&self, file: &str, content: &str) -> String {
        std::fs::write(self.work.join(file), content).expect("write file");
        self.git(&["add", "--all"]);
        self.git(&["commit", "--quiet", "-m", &format!("update {file}")]);
        let branch = self.git(&["rev-parse", "--abbrev-ref", "HEAD"]);
        self.git(&["push", "--quiet", "origin", &branch]);
        self.git(&["rev-parse", "HEAD"])
    }

    /// Creates `branch` from the current commit and pushes it.
    pub fn branch(&self, branch: &str) {
        self.git(&["branch", "--force", branch]);
        self.git(&["push", "--quiet", "--force", "origin", branch]);
    }

    pub fn delete_branch(&self, branch: &str) {
        self.git(&["push", "--quiet", "origin", "--delete", branch]);
    }

    pub fn tag(&self, tag: &str) {
        self.git(&["tag", "--force", tag]);
        self.git(&[
            "push",
            "--quiet",
            "--force",
            "origin",
            &format!("refs/tags/{tag}"),
        ]);
    }

    pub fn annotated_tag(&self, tag: &str) {
        self.git(&["tag", "--force", "-a", "-m", tag, tag]);
        self.git(&[
            "push",
            "--quiet",
            "--force",
            "origin",
            &format!("refs/tags/{tag}"),
        ]);
    }

    pub fn delete_tag(&self, tag: &str) {
        self.git(&[
            "push",
            "--quiet",
            "origin",
            "--delete",
            &format!("refs/tags/{tag}"),
        ]);
    }

    /// Replaces the tip commit of the current branch and force-pushes: the
    /// old tip is no longer an ancestor of the new one.
    pub fn rewrite_tip(&self, file: &str, content: &str) -> String {
        std::fs::write(self.work.join(file), content).expect("write file");
        self.git(&["add", "--all"]);
        self.git(&["commit", "--quiet", "--amend", "-m", "rewritten"]);
        let branch = self.git(&["rev-parse", "--abbrev-ref", "HEAD"]);
        self.git(&["push", "--quiet", "--force", "origin", &branch]);
        self.git(&["rev-parse", "HEAD"])
    }

    /// Renames the default branch the way GitHub does: the new branch
    /// appears, `HEAD` moves to it, and the old branch is gone.
    pub fn rename_default_branch(&self, from: &str, to: &str) {
        self.git(&["branch", "--move", from, to]);
        self.git(&["push", "--quiet", "origin", to]);
        git(
            &self.bare,
            &["symbolic-ref", "HEAD", &format!("refs/heads/{to}")],
        );
        self.git(&["push", "--quiet", "origin", "--delete", from]);
    }

    /// Deletes every branch, leaving tags: GitHub "reports zero branches".
    pub fn delete_all_branches(&self) {
        let heads = git(
            &self.bare,
            &["for-each-ref", "--format=%(refname)", "refs/heads"],
        );
        for head in heads.lines() {
            git(&self.bare, &["update-ref", "-d", head]);
        }
    }

    pub fn refs(&self) -> RefMap {
        ref_map(&self.bare)
    }
}

/// Wraps the real runner: logs every operation and can inject failures.
pub struct LoggingGit {
    inner: GitRunner,
    events: Events,
    /// Replace the LFS commands with logged no-ops. On unless
    /// `FERRY_TEST_LFS=1`, so the flow tests do not need git-lfs.
    pub stub_lfs: AtomicBool,
    pub fail_lfs_push: AtomicBool,
    /// Makes `fetch` hang until the runner is cancelled, like a git child
    /// that only stops when shutdown kills its process group.
    pub hang_fetch: AtomicBool,
    /// Runs once, just before the next real fetch.
    pub before_fetch: Hook,
    /// Runs once, right after the next successful ref push.
    pub after_push: Hook,
    cancel: CancellationToken,
}

/// A one-shot callback a test arms to change the world mid-sync.
#[derive(Default)]
pub struct Hook(Mutex<Option<Box<dyn FnOnce() + Send>>>);

impl Hook {
    pub fn arm(&self, action: impl FnOnce() + Send + 'static) {
        *lock(&self.0) = Some(Box::new(action));
    }

    fn fire(&self) {
        let action = lock(&self.0).take();
        if let Some(action) = action {
            action();
        }
    }
}

impl LoggingGit {
    fn injected(operation: &'static str) -> GitError {
        GitError {
            kind: GitErrorKind::Other,
            operation,
            exit_code: Some(1),
            stderr: "injected failure".to_string(),
        }
    }
}

#[async_trait]
impl Git for LoggingGit {
    async fn ls_remote(&self, remote: &Remote) -> Result<RemoteState, GitError> {
        self.events
            .push(format!("git:ls_remote {}", remote.side.as_str()));
        self.inner.ls_remote(remote).await
    }

    async fn ensure_cache(&self, path: &Path) -> Result<(), GitError> {
        self.events.push("git:ensure_cache");
        self.inner.ensure_cache(path).await
    }

    async fn fetch(&self, path: &Path, remote: &Remote) -> Result<(), GitError> {
        self.events.push("git:fetch");
        if self.hang_fetch.load(Ordering::SeqCst) {
            self.cancel.cancelled().await;
            return Err(GitError {
                kind: GitErrorKind::Cancelled,
                operation: "fetch",
                exit_code: None,
                stderr: String::new(),
            });
        }
        self.before_fetch.fire();
        self.inner.fetch(path, remote).await
    }

    async fn local_refs(&self, path: &Path) -> Result<RefMap, GitError> {
        self.events.push("git:local_refs");
        self.inner.local_refs(path).await
    }

    async fn lfs_fetch(&self, path: &Path, remote: &Remote) -> Result<(), GitError> {
        self.events.push("git:lfs_fetch");
        if self.stub_lfs.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.inner.lfs_fetch(path, remote).await
    }

    async fn lfs_push(&self, path: &Path, remote: &Remote) -> Result<(), GitError> {
        self.events.push("git:lfs_push");
        if self.fail_lfs_push.load(Ordering::SeqCst) {
            return Err(Self::injected("lfs_push"));
        }
        if self.stub_lfs.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.inner.lfs_push(path, remote).await
    }

    async fn push(&self, path: &Path, remote: &Remote, prune: bool) -> Result<(), GitError> {
        self.events.push(if prune {
            "git:push --prune"
        } else {
            "git:push"
        });
        self.inner.push(path, remote, prune).await?;
        self.after_push.fire();
        Ok(())
    }
}

fn logging_git(
    config: &Config,
    token_files: &TokenFiles,
    events: &Events,
    cancel: &CancellationToken,
) -> Arc<LoggingGit> {
    let runner = GitRunner::new(GitSettings {
        cache_dir: config.sync.cache_dir.clone(),
        timeout: config.sync.git_timeout(),
        kill_grace: Duration::from_millis(500),
        token_files: token_files.clone(),
        github_host: "github.invalid".to_string(),
        forgejo_host: "forgejo.invalid".to_string(),
        forgejo_user: FORGEJO_LOGIN.to_string(),
        secrets: vec![Token::new(FORGEJO_TOKEN)],
        askpass_path: PathBuf::from(env!("CARGO_BIN_EXE_ferry")),
        git_program: PathBuf::from("git"),
        cancel: cancel.clone(),
    });
    let lfs_enabled = std::env::var("FERRY_TEST_LFS").is_ok_and(|value| value == "1");
    Arc::new(LoggingGit {
        inner: runner,
        events: events.clone(),
        stub_lfs: AtomicBool::new(!lfs_enabled),
        fail_lfs_push: AtomicBool::new(false),
        hang_fetch: AtomicBool::new(false),
        before_fetch: Hook::default(),
        after_push: Hook::default(),
        cancel: cancel.clone(),
    })
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
        let bare = self
            .root
            .path()
            .join("github")
            .join(owner)
            .join(format!("{name}.git"));
        std::fs::create_dir_all(&bare).expect("create source dir");
        git(
            &bare,
            &[
                "-c",
                "init.defaultBranch=main",
                "init",
                "--bare",
                "--quiet",
                ".",
            ],
        );
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
        std::fs::create_dir_all(&bare).expect("create source dir");
        git(
            &bare,
            &[
                "-c",
                "init.defaultBranch=main",
                "init",
                "--bare",
                "--quiet",
                ".",
            ],
        );
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

    pub fn syncer(&self) -> Arc<RepoSyncer> {
        Arc::new(RepoSyncer::new(self.context()))
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
        self.context().cache_path(entry)
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
