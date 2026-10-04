//! A git wrapper that logs every operation and can inject failures.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferry::config::{Config, TokenFiles};
use ferry::git::{
    Git, GitError, GitErrorKind, GitRunner, GitSettings, RefMap, Remote, RemoteState,
};
use tokio_util::sync::CancellationToken;

use super::git::git_settings;
use super::{Events, lock};

/// Wraps the real runner: logs every operation and can inject failures.
pub struct LoggingGit {
    pub inner: GitRunner,
    pub events: Events,
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
    pub cancel: CancellationToken,
}

/// A one-shot callback a test arms to change the world mid-sync.
#[derive(Default)]
pub struct Hook(Mutex<Option<Box<dyn FnOnce() + Send>>>);

impl Hook {
    pub fn arm(&self, action: impl FnOnce() + Send + 'static) {
        *lock(&self.0) = Some(Box::new(action));
    }

    pub fn fire(&self) {
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

pub fn logging_git(
    config: &Config,
    token_files: &TokenFiles,
    events: &Events,
    cancel: &CancellationToken,
) -> Arc<LoggingGit> {
    let runner = GitRunner::new(GitSettings {
        token_files: token_files.clone(),
        ..git_settings(
            config.sync.cache_dir.clone(),
            config.sync.git_timeout(),
            cancel.clone(),
        )
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
