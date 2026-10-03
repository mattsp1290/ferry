//! The `run` and `sync --once` commands: startup checks, then the scheduler
//! or a single pass.
//!
//! Startup checks decide the exit code. A problem the operator fixes in
//! configuration (bad config, missing token file, missing `git`) exits `2`.
//! A problem with the environment at runtime (unwritable cache, port in use,
//! Forgejo unreachable in `sync --once`) exits `1`.

use std::fmt::Display;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

use crate::cli::exit;
use crate::config::{Config, ConfigError, RepoEntry, TokenFiles, Tokens};
use crate::emitter::{EmitterConfig, run_emitter};
use crate::forge::{ForgejoClient, GithubClient, http_client};
use crate::git::{Git, GitErrorKind, GitRunner};
use crate::health::{self, HealthState};
use crate::scheduler::{Scheduler, SchedulerConfig, SharedStatus, Syncer, run_once};
use crate::sync::SyncContext;
use crate::telemetry::{self, Metrics, Settings};

/// How long process exit waits for blocking tasks that are still running.
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// A failed command and the exit code it maps to.
#[derive(Debug)]
pub struct Failure {
    code: u8,
    message: String,
}

impl Failure {
    fn config(message: impl Display) -> Self {
        Self {
            code: exit::CONFIG,
            message: message.to_string(),
        }
    }

    fn runtime(message: impl Display) -> Self {
        Self {
            code: exit::RUNTIME,
            message: message.to_string(),
        }
    }
}

/// Loads and validates the config. Prints every violation on failure.
pub fn load_config(path: &Path) -> Result<Config, u8> {
    Config::load(path)
        .and_then(|config| config.validate().map(|()| config))
        .map_err(|error| {
            match &error {
                ConfigError::Invalid(violations) => {
                    for violation in violations {
                        eprintln!("ferry: invalid config: {violation}");
                    }
                }
                other => eprintln!("ferry: {other}"),
            }
            exit::CONFIG
        })
}

/// `ferry run`: the scheduler and the health server, until SIGTERM or SIGINT.
pub fn run(config_path: &Path) -> u8 {
    let Ok(config) = load_config(config_path) else {
        return exit::CONFIG;
    };
    let entries = config.repos.clone();
    execute(config, entries, Mode::Run)
}

/// `ferry sync --once`: one pass over the selected entries.
pub fn sync_once(config_path: &Path, repo_filters: &[String]) -> u8 {
    let Ok(config) = load_config(config_path) else {
        return exit::CONFIG;
    };
    let entries = match config.select_repos(repo_filters) {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!("ferry: {error}");
            return exit::CONFIG;
        }
    };
    execute(config, entries, Mode::Once)
}

#[derive(Clone, Copy)]
enum Mode {
    Run,
    Once,
}

fn execute(config: Config, entries: Vec<RepoEntry>, mode: Mode) -> u8 {
    let mut telemetry = telemetry::init(Settings::from_env());
    let metrics = telemetry.metrics();

    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| Failure::runtime(format!("cannot start the async runtime: {error}")))
        .and_then(|runtime| {
            let result = runtime.block_on(start(&config, entries, mode, metrics));
            // Dropping the runtime would wait for a blocking cache size scan,
            // which can take minutes on a large cache.
            runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
            result
        });

    let code = match result {
        Ok(code) => code,
        Err(failure) => {
            tracing::error!(exit_code = failure.code, "{}", failure.message);
            // Logs are JSON on stdout. Say it on stderr too, for a terminal.
            eprintln!("ferry: {}", failure.message);
            failure.code
        }
    };
    // Outside the runtime: flushing the tracer blocks.
    telemetry.shutdown();
    code
}

async fn start(
    config: &Config,
    entries: Vec<RepoEntry>,
    mode: Mode,
    metrics: Arc<dyn Metrics>,
) -> Result<u8, Failure> {
    let token_files = TokenFiles::from_env();
    let tokens = token_files.load().map_err(Failure::config)?;
    // Cancelling this token makes the git runner kill its child process
    // groups. It is separate from the shutdown request so that in-flight
    // syncs get their grace period first.
    let cancel_syncs = CancellationToken::new();

    let runner = GitRunner::from_config(config, &token_files, &tokens, cancel_syncs.clone())
        .map_err(Failure::runtime)?;
    // The cache directory comes first: the git runner keeps its HOME there,
    // so an unusable cache would otherwise be reported as a missing git.
    check_cache_dir(config).await?;
    check_tools(&runner, &entries).await?;

    let syncer = Arc::new(sync_context(config, runner, &tokens)?);
    match mode {
        Mode::Run => serve(config, entries, syncer, metrics, cancel_syncs).await,
        Mode::Once => once(config, entries, syncer, metrics, cancel_syncs).await,
    }
}

fn sync_context(
    config: &Config,
    runner: GitRunner,
    tokens: &Tokens,
) -> Result<SyncContext, Failure> {
    let http = http_client()
        .map_err(|error| Failure::runtime(format!("cannot build the HTTP client: {error}")))?;
    let github = GithubClient::new(http.clone(), &config.github.api_url, tokens.github.clone());
    let forgejo = ForgejoClient::new(http, &config.forgejo.url, tokens.forgejo.clone());
    Ok(SyncContext::new(
        config,
        Arc::new(runner) as Arc<dyn Git>,
        github,
        forgejo,
    ))
}

/// `git` must run. `git-lfs` must run when an entry mirrors LFS objects.
async fn check_tools(runner: &GitRunner, entries: &[RepoEntry]) -> Result<(), Failure> {
    let version = runner.git_version().await.map_err(|error| {
        Failure::config(format!(
            "git is required but `git --version` failed: {error}"
        ))
    })?;
    tracing::info!(git = %version, "found git");

    if entries.iter().any(|entry| entry.lfs) {
        let version = runner.lfs_version().await.map_err(|error| {
            let hint = if error.kind == GitErrorKind::Other {
                " (is git-lfs installed?)"
            } else {
                ""
            };
            Failure::config(format!(
                "git-lfs is required because an entry has lfs = true, but `git lfs version` failed{hint}: {error}"
            ))
        })?;
        tracing::info!(git_lfs = %version, "found git-lfs");
    }
    Ok(())
}

/// The cache directory must exist, or be creatable, and be writable.
async fn check_cache_dir(config: &Config) -> Result<(), Failure> {
    let dir = &config.sync.cache_dir;
    let unusable = |error: std::io::Error| {
        Failure::runtime(format!(
            "cache directory {} is not usable: {error}",
            dir.display()
        ))
    };
    tokio::fs::create_dir_all(dir).await.map_err(unusable)?;
    let probe = dir.join(".ferry-write-probe");
    tokio::fs::write(&probe, b"").await.map_err(unusable)?;
    tokio::fs::remove_file(&probe).await.map_err(unusable)?;
    Ok(())
}

async fn serve(
    config: &Config,
    entries: Vec<RepoEntry>,
    syncer: Arc<dyn Syncer>,
    metrics: Arc<dyn Metrics>,
    cancel_syncs: CancellationToken,
) -> Result<u8, Failure> {
    let listener = tokio::net::TcpListener::bind(&config.health.listen)
        .await
        .map_err(|error| {
            Failure::runtime(format!(
                "cannot bind the health server to {}: {error}",
                config.health.listen
            ))
        })?;
    let shutdown = shutdown_signal()?;
    // Outlives the scheduler so that /healthz answers during the drain.
    let background = CancellationToken::new();

    let health = HealthState::new();
    let health_server = tokio::spawn(health::serve(listener, health.clone(), background.clone()));
    let shared = SharedStatus::new(&entries);
    let emitter = tokio::spawn(run_emitter(
        Arc::clone(&metrics),
        health.clone(),
        shared.clone(),
        EmitterConfig::from_config(config),
        background.clone(),
    ));

    tracing::info!(
        repos = entries.len(),
        poll_interval_seconds = config.sync.poll_interval_seconds,
        version = crate::VERSION,
        "ferry started"
    );
    Scheduler::new(
        syncer,
        metrics,
        health,
        SchedulerConfig::from_config(config),
        shared,
    )
    .run(shutdown, cancel_syncs)
    .await;

    background.cancel();
    let _ = emitter.await;
    match health_server.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(error = %error, "health server stopped with an error"),
        Err(error) => tracing::warn!(error = %error, "health server task failed"),
    }
    tracing::info!("ferry stopped");
    Ok(exit::SUCCESS)
}

async fn once(
    config: &Config,
    entries: Vec<RepoEntry>,
    syncer: Arc<dyn Syncer>,
    metrics: Arc<dyn Metrics>,
    cancel_syncs: CancellationToken,
) -> Result<u8, Failure> {
    syncer.check_ready().await.map_err(|reason| {
        Failure::runtime(format!(
            "Forgejo is not reachable with the configured token: {reason}"
        ))
    })?;

    // An interrupt kills the git children instead of leaving them behind.
    let interrupted = shutdown_signal()?;
    let pass = run_once(
        syncer,
        metrics.as_ref(),
        &entries,
        config.sync.max_concurrency,
        &cancel_syncs,
    );
    tokio::pin!(pass);
    let outcomes = tokio::select! {
        outcomes = &mut pass => outcomes,
        () = interrupted.cancelled() => {
            // Stops the running git children and keeps the remaining entries
            // from starting.
            cancel_syncs.cancel();
            pass.await;
            return Err(Failure::runtime("interrupted"));
        }
    };

    let failed = outcomes
        .iter()
        .filter(|outcome| {
            !outcome
                .as_ref()
                .is_some_and(|outcome| outcome.result.is_success())
        })
        .count();
    tracing::info!(repos = entries.len(), failed, "sync pass finished");
    Ok(if failed == 0 {
        exit::SUCCESS
    } else {
        exit::RUNTIME
    })
}

/// A token that is cancelled on the first SIGTERM or SIGINT.
fn shutdown_signal() -> Result<CancellationToken, Failure> {
    let install = |kind: SignalKind| {
        signal(kind)
            .map_err(|error| Failure::runtime(format!("cannot install a signal handler: {error}")))
    };
    let mut terminate = install(SignalKind::terminate())?;
    let mut interrupt = install(SignalKind::interrupt())?;
    let token = CancellationToken::new();
    let trigger = token.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
        tracing::info!("shutdown signal received");
        trigger.cancel();
    });
    Ok(token)
}
