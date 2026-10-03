//! Per-repository sync: make one Forgejo repository an exact mirror of one
//! GitHub repository.
//!
//! Ferry keeps no state that correctness depends on. Every pass compares
//! `git ls-remote` of both sides and repairs whatever differs, so a pass that
//! is interrupted at any step is finished by the next one. The bare cache and
//! the LFS marker in it only save work.

pub mod marker;
pub mod outcome;

use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;
use tracing::Instrument;
use tracing::field::Empty;

use crate::config::{Config, RepoEntry};
use crate::forge::{CreateOutcome, DestRepo, ForgeError, ForgejoClient, GithubClient, RepoEdit};
use crate::git::{Git, GitError, GitErrorKind, RefMap, Remote, RemoteState, Side};
use crate::scheduler::Syncer;
use crate::telemetry;

pub use outcome::{ErrorKind, SyncOutcome, SyncResult};

/// Everything one sync pass needs. Shared by all entries.
pub struct SyncContext {
    git: Arc<dyn Git>,
    github: GithubClient,
    forgejo: ForgejoClient,
    github_git_url: String,
    /// Base URL of Forgejo git transport.
    forgejo_url: String,
    cache_dir: PathBuf,
    metadata_interval: Duration,
    /// When the GitHub REST metadata of an entry was last requested, keyed by
    /// repo tag. Ferry asks at most once per entry per metadata interval.
    metadata_checked: Mutex<HashMap<String, Instant>>,
    /// Entries already warned about for being public on Forgejo.
    public_warned: Mutex<HashSet<String>>,
}

impl SyncContext {
    pub fn new(
        config: &Config,
        git: Arc<dyn Git>,
        github: GithubClient,
        forgejo: ForgejoClient,
    ) -> Self {
        Self {
            git,
            github,
            forgejo,
            github_git_url: config.github.git_url.trim_end_matches('/').to_string(),
            forgejo_url: config.forgejo.url.trim_end_matches('/').to_string(),
            cache_dir: config.sync.cache_dir.clone(),
            metadata_interval: config.sync.metadata_interval(),
            metadata_checked: Mutex::new(HashMap::new()),
            public_warned: Mutex::new(HashSet::new()),
        }
    }

    /// Uses a different base URL for Forgejo git transport than for its API.
    ///
    /// In production both are `forgejo.url`: LFS object URLs are built from
    /// Forgejo's `ROOT_URL`, so the pod must reach that host anyway. Tests
    /// pair a mock API server with `file://` git remotes.
    pub fn with_forgejo_git_url(mut self, url: &str) -> Self {
        self.forgejo_url = url.trim_end_matches('/').to_string();
        self
    }

    /// The bare cache repository of an entry.
    pub fn cache_path(&self, entry: &RepoEntry) -> PathBuf {
        let (owner, name) = entry.github_parts();
        self.cache_dir
            .join("repos")
            .join(owner)
            .join(format!("{name}.git"))
    }

    fn source_remote(&self, entry: &RepoEntry) -> Remote {
        let (owner, name) = entry.github_parts();
        Remote {
            url: format!("{}/{owner}/{name}.git", self.github_git_url),
            side: Side::Github,
        }
    }

    fn dest_remote(&self, entry: &RepoEntry) -> Remote {
        let (owner, name) = entry.forgejo_parts();
        Remote {
            url: format!("{}/{owner}/{name}.git", self.forgejo_url),
            side: Side::Forgejo,
        }
    }

    /// Whether the GitHub REST metadata is due, and if so, records the
    /// attempt. A failed request therefore also waits one interval.
    fn take_metadata_turn(&self, entry: &RepoEntry) -> bool {
        let now = Instant::now();
        let mut checked = self
            .metadata_checked
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let due = checked
            .get(&entry.repo_tag())
            .is_none_or(|last| now.saturating_duration_since(*last) >= self.metadata_interval);
        if due {
            checked.insert(entry.repo_tag(), now);
        }
        due
    }

    fn warn_public_once(&self, entry: &RepoEntry) {
        let first = self
            .public_warned
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(entry.repo_tag());
        if first {
            tracing::warn!(
                forgejo_repo = %entry.forgejo,
                "existing Forgejo repository is public; ferry does not change visibility"
            );
        }
    }
}

/// Adapts the sync engine to the scheduler.
pub struct RepoSyncer {
    ctx: SyncContext,
}

impl RepoSyncer {
    pub fn new(ctx: SyncContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Syncer for RepoSyncer {
    async fn check_ready(&self) -> Result<(), String> {
        self.ctx
            .forgejo
            .whoami()
            .await
            .map(drop)
            .map_err(|error| error.to_string())
    }

    async fn sync(&self, entry: &RepoEntry) -> SyncOutcome {
        sync_repo(&self.ctx, entry).await
    }
}

/// Why a pass stopped. `detail` is safe to log: git stderr in it is already
/// truncated and redacted, and forge errors never carry a credential.
struct Failure {
    kind: ErrorKind,
    retry_after: Option<Duration>,
    detail: String,
}

impl Failure {
    fn new(kind: ErrorKind, detail: impl Display) -> Self {
        Self {
            kind,
            retry_after: None,
            detail: detail.to_string(),
        }
    }

    /// A git failure while reading GitHub.
    fn source(error: GitError) -> Self {
        let kind = match error.kind {
            GitErrorKind::NotFound => ErrorKind::SourceMissing,
            GitErrorKind::Auth => ErrorKind::SourceAuth,
            GitErrorKind::Timeout => ErrorKind::Timeout,
            GitErrorKind::Network => ErrorKind::Network,
            GitErrorKind::Rejected | GitErrorKind::Cancelled | GitErrorKind::Other => {
                ErrorKind::Internal
            }
        };
        Self::new(kind, error)
    }

    /// A git failure while reading or writing Forgejo.
    fn dest(error: GitError) -> Self {
        let kind = match error.kind {
            GitErrorKind::Auth => ErrorKind::DestAuth,
            GitErrorKind::Rejected => ErrorKind::DestRejected,
            GitErrorKind::Timeout => ErrorKind::Timeout,
            GitErrorKind::Network => ErrorKind::Network,
            GitErrorKind::NotFound | GitErrorKind::Cancelled | GitErrorKind::Other => {
                ErrorKind::Internal
            }
        };
        Self::new(kind, error)
    }

    /// A git failure in the local cache repository.
    fn cache(error: GitError) -> Self {
        let kind = match error.kind {
            GitErrorKind::Timeout => ErrorKind::Timeout,
            _ => ErrorKind::Internal,
        };
        Self::new(kind, error)
    }

    /// A Forgejo REST failure outside metadata reconciliation.
    fn forgejo(error: ForgeError) -> Self {
        let mut failure = Self::new(ErrorKind::Internal, &error);
        match error {
            ForgeError::Auth { .. } => failure.kind = ErrorKind::DestAuth,
            ForgeError::RateLimited { retry_after, .. } => {
                failure.kind = ErrorKind::RateLimited;
                failure.retry_after = retry_after;
            }
            // Forgejo answered but cannot serve the request right now. For
            // alerting this is the same condition as not reaching it.
            ForgeError::Network { .. } | ForgeError::Server { .. } => {
                failure.kind = ErrorKind::Network;
            }
            // Creating under an owner that does not exist, for example.
            ForgeError::NotFound { .. } => failure.kind = ErrorKind::DestRejected,
            ForgeError::Unexpected { .. } => {}
        }
        failure
    }
}

/// What a successful pass did.
struct Done {
    result: SyncResult,
    refs_changed: u32,
    refs_pruned: u32,
}

impl Done {
    fn unchanged(result: SyncResult) -> Self {
        Self {
            result,
            refs_changed: 0,
            refs_pruned: 0,
        }
    }
}

/// Runs one pass for one entry inside a `ferry.sync_repo` root span and logs
/// one line with the result.
pub async fn sync_repo(ctx: &SyncContext, entry: &RepoEntry) -> SyncOutcome {
    let span = tracing::info_span!(
        parent: None,
        "ferry.sync_repo",
        repo = %entry.repo_tag(),
        forgejo_repo = %entry.forgejo,
        result = Empty,
        error_kind = Empty,
        refs_changed = Empty,
    );
    async {
        let started = Instant::now();
        let finished = run(ctx, entry).await;
        let duration = started.elapsed();
        let duration_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);

        let span = tracing::Span::current();
        let outcome = match finished {
            Ok(done) => {
                let mut outcome = SyncOutcome::success(done.result, duration);
                outcome.refs_changed = done.refs_changed;
                outcome.refs_pruned = done.refs_pruned;
                outcome
            }
            Err(failure) => {
                telemetry::tracing::mark_error(&span, failure.kind.as_str());
                tracing::error!(
                    result = SyncResult::Error.as_str(),
                    error_kind = failure.kind.as_str(),
                    duration_ms,
                    detail = %failure.detail,
                    "sync failed"
                );
                let mut outcome = SyncOutcome::error(failure.kind, duration);
                outcome.retry_after = failure.retry_after;
                outcome
            }
        };

        span.record("result", outcome.result.as_str());
        span.record("error_kind", outcome.error_kind_tag());
        span.record("refs_changed", outcome.refs_changed);
        match outcome.result {
            SyncResult::Error => {}
            // Most passes change nothing. Logging them at info would bury
            // the lines that matter.
            SyncResult::Noop => tracing::debug!(
                result = outcome.result.as_str(),
                error_kind = outcome.error_kind_tag(),
                duration_ms,
                refs_changed = outcome.refs_changed,
                "sync finished"
            ),
            SyncResult::Synced | SyncResult::Empty => tracing::info!(
                result = outcome.result.as_str(),
                error_kind = outcome.error_kind_tag(),
                duration_ms,
                refs_changed = outcome.refs_changed,
                refs_pruned = outcome.refs_pruned,
                "sync finished"
            ),
        }
        outcome
    }
    .instrument(span)
    .await
}

async fn run(ctx: &SyncContext, entry: &RepoEntry) -> Result<Done, Failure> {
    let (owner, name) = entry.forgejo_parts();
    let source = ctx.source_remote(entry);
    let dest = ctx.dest_remote(entry);
    let cache = ctx.cache_path(entry);

    let src = ctx.git.ls_remote(&source).await.map_err(Failure::source)?;

    let (dest_repo, dst) = match ctx
        .forgejo
        .get_repo(owner, name)
        .await
        .map_err(Failure::forgejo)?
    {
        Some(repo) => {
            let dst = inspect_existing(ctx, entry, &repo, &dest).await?;
            (repo, dst)
        }
        None => provision(ctx, entry, &dest).await?,
    };

    if !src.refs.has_heads() {
        return if dst.refs.is_empty() {
            Ok(Done::unchanged(SyncResult::Empty))
        } else {
            // Never prune a populated mirror because GitHub reports zero
            // branches: that is far more likely a fault than an intent.
            Err(Failure::new(
                ErrorKind::SourceEmpty,
                "GitHub reports no branches but the Forgejo repository has refs",
            ))
        };
    }

    let lfs_current = !entry.lfs || marker::is_current(&cache, &src.refs.hash()).await;
    if src.refs == dst.refs && lfs_current {
        reconcile_metadata(ctx, entry, &dest_repo, src.head.as_deref(), &dst.refs).await?;
        return Ok(Done::unchanged(SyncResult::Noop));
    }

    ctx.git.ensure_cache(&cache).await.map_err(Failure::cache)?;
    ctx.git
        .fetch(&cache, &source)
        .await
        .map_err(Failure::source)?;
    // GitHub can change between the first ls-remote and the fetch. From here
    // on the fetched refs are the source of truth for this pass.
    let local = ctx.git.local_refs(&cache).await.map_err(Failure::cache)?;
    if !local.has_heads() {
        return Err(Failure::new(
            ErrorKind::SourceEmpty,
            "the fetch from GitHub returned no branches",
        ));
    }

    // LFS objects go first: a crash after this step repeats the sync, while
    // refs pushed first would point at objects Forgejo does not have.
    if entry.lfs {
        ctx.git
            .lfs_fetch(&cache, &source)
            .await
            .map_err(|error| Failure::new(ErrorKind::Lfs, error))?;
        ctx.git
            .lfs_push(&cache, &dest)
            .await
            .map_err(|error| Failure::new(ErrorKind::Lfs, error))?;
    }

    ctx.git
        .push(&cache, &dest, false)
        .await
        .map_err(Failure::dest)?;

    // Push, then switch the default branch, then prune. Forgejo refuses to
    // delete its default branch, so a renamed default branch must be switched
    // before the old one is pruned.
    reconcile_metadata(ctx, entry, &dest_repo, src.head.as_deref(), &local).await?;

    let refs_pruned = count(dst.refs.missing_in(&local).len());
    if refs_pruned > 0 {
        ctx.git
            .push(&cache, &dest, true)
            .await
            .map_err(Failure::dest)?;
    }

    let verified = ctx.git.ls_remote(&dest).await.map_err(Failure::dest)?;
    if verified.refs != local {
        return Err(Failure::new(
            ErrorKind::VerifyMismatch,
            format!(
                "after the push {} Forgejo refs differ from the fetched GitHub refs",
                verified.refs.diff_count(&local)
            ),
        ));
    }
    if entry.lfs {
        marker::write(&cache, &local.hash())
            .await
            .map_err(|error| {
                Failure::new(
                    ErrorKind::Internal,
                    format!("cannot write the LFS marker: {error}"),
                )
            })?;
    }

    Ok(Done {
        result: SyncResult::Synced,
        refs_changed: count(dst.refs.diff_count(&local)),
        refs_pruned,
    })
}

fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// The destination exists. Returns its refs once ferry may write to it.
async fn inspect_existing(
    ctx: &SyncContext,
    entry: &RepoEntry,
    repo: &DestRepo,
    dest: &Remote,
) -> Result<RemoteState, Failure> {
    let (owner, name) = entry.forgejo_parts();
    if repo.mirror {
        // A pull mirror is read-only and owned by Forgejo's own mirror job.
        return Err(Failure::new(
            ErrorKind::DestIsPullMirror,
            "the Forgejo repository is a pull mirror",
        ));
    }
    if !repo.private {
        ctx.warn_public_once(entry);
    }

    let dst = ctx.git.ls_remote(dest).await.map_err(Failure::dest)?;
    let managed = ctx
        .forgejo
        .has_marker(owner, name)
        .await
        .map_err(Failure::forgejo)?;
    if !managed {
        // Only a marked repository is ferry's to overwrite. An empty one has
        // nothing to lose, and `adopt` is the owner's explicit consent.
        if !entry.adopt && !dst.refs.is_empty() {
            return Err(Failure::new(
                ErrorKind::DestUnmanaged,
                format!(
                    "the Forgejo repository has refs but no {} topic; set adopt = true to take it over",
                    crate::forge::MARKER_TOPIC
                ),
            ));
        }
        ctx.forgejo
            .add_marker(owner, name)
            .await
            .map_err(Failure::forgejo)?;
    }
    Ok(dst)
}

/// The destination is missing: create it private, mark it, set Actions.
async fn provision(
    ctx: &SyncContext,
    entry: &RepoEntry,
    dest: &Remote,
) -> Result<(DestRepo, RemoteState), Failure> {
    let (owner, name) = entry.forgejo_parts();
    let description = source_description(ctx, entry).await.unwrap_or_default();

    let (repo, created) = ctx
        .forgejo
        .create_repo(owner, name, &description)
        .await
        .map_err(Failure::forgejo)?;
    if created == CreateOutcome::AlreadyExisted {
        // Someone created it between the lookup and the create call. It gets
        // no special treatment: the existing-repository rules apply.
        let dst = inspect_existing(ctx, entry, &repo, dest).await?;
        return Ok((repo, dst));
    }

    ctx.forgejo
        .add_marker(owner, name)
        .await
        .map_err(Failure::forgejo)?;
    // Forgejo would otherwise run the mirrored repository's workflows.
    let actions = RepoEdit {
        has_actions: Some(entry.actions),
        ..RepoEdit::default()
    };
    ctx.forgejo
        .edit_repo(owner, name, &actions)
        .await
        .map_err(Failure::forgejo)?;
    tracing::info!(forgejo_repo = %entry.forgejo, "created Forgejo repository");

    Ok((repo, RemoteState::default()))
}

/// The GitHub description, when the REST call is due and succeeds. A failure
/// never fails the sync: the description stays as it is until the next
/// metadata interval.
async fn source_description(ctx: &SyncContext, entry: &RepoEntry) -> Option<String> {
    if !ctx.take_metadata_turn(entry) {
        return None;
    }
    let (owner, name) = entry.github_parts();
    match ctx.github.get_repo(owner, name).await {
        Ok(meta) => Some(meta.description),
        Err(error) => {
            tracing::warn!(error = %error, "cannot read GitHub repository metadata; keeping the description");
            None
        }
    }
}

/// Aligns the Forgejo default branch and description with GitHub.
///
/// The default branch follows GitHub's `HEAD` symref from this pass, and only
/// once that branch exists on Forgejo (`present`).
async fn reconcile_metadata(
    ctx: &SyncContext,
    entry: &RepoEntry,
    dest_repo: &DestRepo,
    source_head: Option<&str>,
    present: &RefMap,
) -> Result<(), Failure> {
    let (owner, name) = entry.forgejo_parts();
    let mut edit = RepoEdit::default();

    if let Some(branch) = source_head
        && present.has_head(branch)
        && dest_repo.default_branch != branch
    {
        edit.default_branch = Some(branch.to_string());
    }
    if let Some(description) = source_description(ctx, entry).await
        && description != dest_repo.description
    {
        edit.description = Some(description);
    }

    ctx.forgejo
        .edit_repo(owner, name, &edit)
        .await
        .map_err(|error| Failure::new(ErrorKind::Metadata, error))
}
