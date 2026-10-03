//! `git` and `git-lfs` child processes. Every git operation goes through this module.
//!
//! `GitRunner` owns the child's environment, process group, timeout, stderr
//! redaction, and error classification. Remote URLs are passed per command and
//! never contain userinfo. Credentials reach git through askpass mode
//! (`GIT_ASKPASS` points at the ferry binary), so no token appears in argv, a
//! URL, or `.git/config`.

pub mod askpass;
mod error;
pub mod refs;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, Span, field};
use url::Url;

use crate::config::{
    Config, FORGEJO_TOKEN_FILE_ENV, GITHUB_TOKEN_FILE_ENV, Token, TokenFiles, Tokens,
};
pub use error::{GitError, GitErrorKind, STDERR_LIMIT, classify, sanitize_stderr};
pub use refs::{RefMap, RemoteState};

/// Default wait between SIGTERM and SIGKILL.
pub const DEFAULT_KILL_GRACE: Duration = Duration::from_secs(10);

/// Most stderr read from a child; the rest is drained and discarded.
const STDERR_READ_CAP: usize = 1 << 20;
const STDOUT_READ_CAP: usize = 64 << 20;
/// How long to wait for a pipe to close after the child has exited.
const PIPE_DRAIN_WAIT: Duration = Duration::from_secs(2);

const FETCH_REFSPECS: [&str; 2] = ["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Github,
    Forgejo,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Forgejo => "forgejo",
        }
    }
}

/// A remote repository URL, without userinfo, and the side it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub url: String,
    pub side: Side,
}

/// The host and port of a remote URL, for the askpass host variables.
pub fn host_of(url: &str) -> Option<String> {
    askpass::host_port(url)
}

/// Everything a `GitRunner` needs. Plain data: the runner reads no global
/// environment except `PATH`.
#[derive(Debug, Clone)]
pub struct GitSettings {
    pub cache_dir: PathBuf,
    /// Applied to each child process.
    pub timeout: Duration,
    /// Wait between SIGTERM and SIGKILL.
    pub kill_grace: Duration,
    pub token_files: TokenFiles,
    /// `host[:port]` that askpass answers with the GitHub token.
    pub github_host: String,
    /// `host[:port]` that askpass answers with the Forgejo token.
    pub forgejo_host: String,
    pub forgejo_user: String,
    /// Secret values to strip from stderr.
    pub secrets: Vec<Token>,
    /// The executable git runs as `GIT_ASKPASS`.
    pub askpass_path: PathBuf,
    /// The `git` program. Overridable for tests.
    pub git_program: PathBuf,
    pub cancel: CancellationToken,
}

impl GitSettings {
    /// Derives settings from the configuration. The askpass path is the
    /// current executable; override `askpass_path` where that is not ferry.
    pub fn from_config(
        config: &Config,
        token_files: &TokenFiles,
        tokens: &Tokens,
        cancel: CancellationToken,
    ) -> std::io::Result<Self> {
        let invalid = |what: &str, url: &str| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{what} has no host: {url}"),
            )
        };
        Ok(Self {
            cache_dir: config.sync.cache_dir.clone(),
            timeout: config.sync.git_timeout(),
            kill_grace: DEFAULT_KILL_GRACE,
            token_files: token_files.clone(),
            github_host: host_of(&config.github.git_url)
                .ok_or_else(|| invalid("github.git_url", &config.github.git_url))?,
            forgejo_host: host_of(&config.forgejo.url)
                .ok_or_else(|| invalid("forgejo.url", &config.forgejo.url))?,
            forgejo_user: config.forgejo.username.clone(),
            secrets: tokens.secrets(),
            askpass_path: std::env::current_exe()?,
            git_program: PathBuf::from("git"),
            cancel,
        })
    }
}

/// The git operations the sync engine needs.
#[async_trait::async_trait]
pub trait Git: Send + Sync {
    async fn ls_remote(&self, remote: &Remote) -> Result<RemoteState, GitError>;
    async fn ensure_cache(&self, path: &Path) -> Result<(), GitError>;
    async fn fetch(&self, path: &Path, remote: &Remote) -> Result<(), GitError>;
    async fn local_refs(&self, path: &Path) -> Result<RefMap, GitError>;
    async fn lfs_fetch(&self, path: &Path, remote: &Remote) -> Result<(), GitError>;
    async fn lfs_push(&self, path: &Path, remote: &Remote) -> Result<(), GitError>;
    async fn push(&self, path: &Path, remote: &Remote, prune: bool) -> Result<(), GitError>;
}

/// Runs the real `git` and `git-lfs`.
#[derive(Debug, Clone)]
pub struct GitRunner {
    settings: GitSettings,
}

/// An `info` span for one git operation. `git.side` is present when the
/// operation talks to a remote. `exec` fills in `git.exit_code`.
macro_rules! git_span {
    ($name:literal) => {
        tracing::info_span!($name, git.exit_code = field::Empty)
    };
    ($name:literal, $remote:expr) => {
        tracing::info_span!(
            $name,
            git.side = $remote.side.as_str(),
            git.exit_code = field::Empty
        )
    };
}

impl GitRunner {
    pub fn new(settings: GitSettings) -> Self {
        Self { settings }
    }

    /// See [`GitSettings::from_config`].
    pub fn from_config(
        config: &Config,
        token_files: &TokenFiles,
        tokens: &Tokens,
        cancel: CancellationToken,
    ) -> std::io::Result<Self> {
        GitSettings::from_config(config, token_files, tokens, cancel).map(Self::new)
    }

    /// Overrides the askpass executable. Integration tests pass
    /// `env!("CARGO_BIN_EXE_ferry")`, because `current_exe()` there is the
    /// test binary.
    pub fn with_askpass_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.settings.askpass_path = path.into();
        self
    }

    pub fn settings(&self) -> &GitSettings {
        &self.settings
    }

    /// Output of `git --version`.
    pub async fn git_version(&self) -> Result<String, GitError> {
        let out = self
            .run(
                git_span!("git.version"),
                "version",
                None,
                args(["--version"]),
            )
            .await?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    /// Output of `git lfs version`. Separate so callers require git-lfs only
    /// when an entry needs it.
    pub async fn lfs_version(&self) -> Result<String, GitError> {
        let out = self
            .run(
                git_span!("git.lfs_version"),
                "lfs_version",
                None,
                args(["lfs", "version"]),
            )
            .await?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    fn check_remote(operation: &'static str, remote: &Remote) -> Result<(), GitError> {
        if let Ok(url) = Url::parse(&remote.url)
            && (!url.username().is_empty() || url.password().is_some())
        {
            return Err(GitError::other(
                operation,
                "remote URL must not contain userinfo",
            ));
        }
        Ok(())
    }

    /// `--git-dir <path>` rather than `-C`: an invalid cache directory never
    /// falls through to a repository further up the tree.
    fn cache_args<const N: usize>(path: &Path, rest: [&str; N]) -> Vec<OsString> {
        let mut out: Vec<OsString> = vec![{
            let mut flag = OsString::from("--git-dir=");
            flag.push(path);
            flag
        }];
        out.extend(rest.iter().map(OsString::from));
        out
    }

    /// The LFS endpoint that belongs to `remote`.
    fn lfs_endpoint(remote: &Remote) -> String {
        if remote.url.starts_with("http://") || remote.url.starts_with("https://") {
            format!("{}/info/lfs", remote.url.trim_end_matches('/'))
        } else {
            // `file://` remotes, which tests use: the repository itself.
            remote.url.clone()
        }
    }

    /// `git lfs <verb> --all <remote>` with the LFS endpoint pinned.
    ///
    /// git-lfs reads `.lfsconfig` from the cache's `HEAD`, and an `lfs.url`
    /// there outranks the remote argument. Left alone, the content of a
    /// mirrored repository could send ferry's token-authenticated upload to
    /// another repository or host, and the push to Forgejo would "succeed"
    /// having uploaded nothing. Command-line config outranks `.lfsconfig`.
    ///
    /// git-lfs accepts a URL in the remote position, so the cache repository
    /// stores no remote.
    fn lfs_args(path: &Path, verb: &str, remote: &Remote) -> Vec<OsString> {
        let endpoint = Self::lfs_endpoint(remote);
        let mut argv: Vec<OsString> = vec![
            "-c".into(),
            format!("lfs.url={endpoint}").into(),
            "-c".into(),
            format!("lfs.pushurl={endpoint}").into(),
        ];
        argv.extend(Self::cache_args(path, ["lfs", verb, "--all"]));
        argv.push((&remote.url).into());
        argv
    }

    fn child_env(&self) -> Vec<(&'static str, OsString)> {
        let s = &self.settings;
        let mut env: Vec<(&'static str, OsString)> = vec![
            ("GIT_TERMINAL_PROMPT", "0".into()),
            ("GIT_ASKPASS", s.askpass_path.clone().into_os_string()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
            ("HOME", s.cache_dir.join(".home").into_os_string()),
            (askpass::ASKPASS_ENV, "1".into()),
            (askpass::GITHUB_HOST_ENV, s.github_host.clone().into()),
            (askpass::FORGEJO_HOST_ENV, s.forgejo_host.clone().into()),
            (askpass::FORGEJO_USER_ENV, s.forgejo_user.clone().into()),
            (
                "PATH",
                std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into()),
            ),
        ];
        if let Some(path) = &s.token_files.github {
            env.push((GITHUB_TOKEN_FILE_ENV, path.clone().into_os_string()));
        }
        if let Some(path) = &s.token_files.forgejo {
            env.push((FORGEJO_TOKEN_FILE_ENV, path.clone().into_os_string()));
        }
        env
    }

    /// One git operation: rejects a remote URL with userinfo, then runs the
    /// child inside `span`.
    async fn run(
        &self,
        span: Span,
        operation: &'static str,
        remote: Option<&Remote>,
        argv: Vec<OsString>,
    ) -> Result<Vec<u8>, GitError> {
        async {
            if let Some(remote) = remote {
                Self::check_remote(operation, remote)?;
            }
            self.exec(operation, &span, argv).await
        }
        .instrument(span.clone())
        .await
    }

    /// Runs one git child to completion under the timeout and cancellation
    /// rules and returns its stdout. `span` receives `git.exit_code`.
    async fn exec(
        &self,
        operation: &'static str,
        span: &Span,
        git_args: Vec<OsString>,
    ) -> Result<Vec<u8>, GitError> {
        let s = &self.settings;
        if s.cancel.is_cancelled() {
            return Err(GitError {
                kind: GitErrorKind::Cancelled,
                operation,
                exit_code: None,
                stderr: String::new(),
            });
        }
        tokio::fs::create_dir_all(s.cache_dir.join(".home"))
            .await
            .map_err(|error| GitError::other(operation, format!("cannot create HOME: {error}")))?;

        let mut command = Command::new(&s.git_program);
        command
            .arg("-c")
            .arg("credential.helper=")
            .arg("-c")
            .arg("protocol.version=2")
            .args(&git_args)
            .env_clear()
            .envs(self.child_env())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| GitError::other(operation, format!("cannot spawn git: {error}")))?;
        let group = child.id().map(|pid| Pid::from_raw(pid as i32));
        let stdout = child
            .stdout
            .take()
            .map(|pipe| tokio::spawn(read_capped(pipe, STDOUT_READ_CAP)));
        let stderr = child
            .stderr
            .take()
            .map(|pipe| tokio::spawn(read_capped(pipe, STDERR_READ_CAP)));

        let ended: Result<std::io::Result<ExitStatus>, GitErrorKind> = tokio::select! {
            status = child.wait() => Ok(status),
            () = tokio::time::sleep(s.timeout) => Err(GitErrorKind::Timeout),
            () = s.cancel.cancelled() => Err(GitErrorKind::Cancelled),
        };
        let (kind, status) = match ended {
            Ok(status) => (None, status.ok()),
            Err(kind) => {
                terminate(&mut child, group, s.kill_grace).await;
                (Some(kind), None)
            }
        };

        let out = join_pipe(stdout).await;
        let err = join_pipe(stderr).await;
        let exit_code = status.and_then(|status| status.code());
        if let Some(code) = exit_code {
            span.record("git.exit_code", i64::from(code));
        }
        let stderr_text = sanitize_stderr(&err.bytes, &s.secrets);
        if let Some(kind) = kind {
            return Err(GitError {
                kind,
                operation,
                exit_code: None,
                stderr: stderr_text,
            });
        }
        match status {
            Some(status) if status.success() && out.complete => Ok(out.bytes),
            // Fail closed. Returning partial or empty output as success
            // would, for `ls-remote`, read as "the remote has no refs".
            Some(status) if status.success() => Err(GitError {
                kind: GitErrorKind::Other,
                operation,
                exit_code,
                stderr: "git exited 0 but its output could not be read in full".to_string(),
            }),
            _ => Err(GitError {
                kind: classify(&stderr_text),
                operation,
                exit_code,
                stderr: stderr_text,
            }),
        }
    }
}

/// Stops a child that timed out or was cancelled: SIGTERM to its process
/// group, `grace` to exit, then SIGKILL.
async fn terminate(child: &mut Child, group: Option<Pid>, grace: Duration) {
    if let Some(group) = group {
        let _ = killpg(group, Signal::SIGTERM);
    }
    let _ = tokio::time::timeout(grace, child.wait()).await;
    // The leader may have exited while helpers linger, so always finish the
    // group off.
    if let Some(group) = group {
        let _ = killpg(group, Signal::SIGKILL);
    }
    let _ = child.wait().await;
}

fn args<const N: usize>(items: [&str; N]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

/// What a reader task collected from one pipe.
struct Captured {
    bytes: Vec<u8>,
    /// False when a read failed or the output exceeded the cap.
    complete: bool,
}

/// Reads up to `cap` bytes and drains the rest so the child never blocks.
async fn read_capped<R: AsyncRead + Unpin>(mut pipe: R, cap: usize) -> Captured {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut complete = true;
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) => break,
            Err(_) => {
                complete = false;
                break;
            }
            Ok(n) => {
                let room = cap.saturating_sub(bytes.len());
                complete &= n <= room;
                bytes.extend_from_slice(&chunk[..n.min(room)]);
            }
        }
    }
    Captured { bytes, complete }
}

/// Collects a reader task, giving up if a detached grandchild holds the pipe.
/// Whatever was not read in full is reported as incomplete.
async fn join_pipe(task: Option<tokio::task::JoinHandle<Captured>>) -> Captured {
    let incomplete = Captured {
        bytes: Vec::new(),
        complete: false,
    };
    let Some(mut task) = task else {
        return incomplete;
    };
    match tokio::time::timeout(PIPE_DRAIN_WAIT, &mut task).await {
        Ok(Ok(captured)) => captured,
        _ => {
            task.abort();
            incomplete
        }
    }
}

#[async_trait::async_trait]
impl Git for GitRunner {
    async fn ls_remote(&self, remote: &Remote) -> Result<RemoteState, GitError> {
        let argv = args([
            "ls-remote",
            "--symref",
            &remote.url,
            "HEAD",
            "refs/heads/*",
            "refs/tags/*",
        ]);
        let out = self
            .run(
                git_span!("git.ls_remote", remote),
                "ls_remote",
                Some(remote),
                argv,
            )
            .await?;
        Ok(refs::parse_ls_remote(&String::from_utf8_lossy(&out)))
    }

    async fn ensure_cache(&self, path: &Path) -> Result<(), GitError> {
        let span = tracing::info_span!("git.init", git.exit_code = field::Empty);
        async {
            let probe = || {
                self.exec(
                    "rev_parse",
                    &span,
                    Self::cache_args(path, ["rev-parse", "--git-dir"]),
                )
            };
            let init = || {
                let mut init = args(["-c", "init.defaultBranch=main", "init", "--bare", "--quiet"]);
                init.push(path.into());
                self.exec("init", &span, init)
            };
            if tokio::fs::try_exists(path).await.unwrap_or(false) {
                match probe().await {
                    Ok(_) => return Ok(()),
                    // git ran and rejected the directory: a corrupt cache.
                    Err(error) if error.exit_code.is_some() => {}
                    // Cancelled, timed out, or never started. That says
                    // nothing about the cache, and deleting it would cost a
                    // full re-fetch and LFS re-push.
                    Err(error) => return Err(error),
                }
                tokio::fs::remove_dir_all(path).await.map_err(|error| {
                    GitError::other("init", format!("cannot remove corrupt cache: {error}"))
                })?;
            }
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|error| {
                    GitError::other("init", format!("cannot create cache parent: {error}"))
                })?;
            }
            init().await?;
            probe().await.map(|_| ())
        }
        .instrument(span.clone())
        .await
    }

    async fn fetch(&self, path: &Path, remote: &Remote) -> Result<(), GitError> {
        let mut argv = Self::cache_args(path, ["fetch", "--force", "--prune", "--no-tags"]);
        argv.push((&remote.url).into());
        argv.extend(FETCH_REFSPECS.iter().map(OsString::from));
        self.run(git_span!("git.fetch", remote), "fetch", Some(remote), argv)
            .await
            .map(|_| ())
    }

    async fn local_refs(&self, path: &Path) -> Result<RefMap, GitError> {
        let argv = Self::cache_args(
            path,
            [
                "for-each-ref",
                "--format=%(objectname) %(refname)",
                "refs/heads",
                "refs/tags",
            ],
        );
        let out = self
            .run(git_span!("git.local_refs"), "local_refs", None, argv)
            .await?;
        Ok(refs::parse_for_each_ref(&String::from_utf8_lossy(&out)))
    }

    async fn lfs_fetch(&self, path: &Path, remote: &Remote) -> Result<(), GitError> {
        let argv = Self::lfs_args(path, "fetch", remote);
        self.run(
            git_span!("git.lfs_fetch", remote),
            "lfs_fetch",
            Some(remote),
            argv,
        )
        .await
        .map(|_| ())
    }

    async fn lfs_push(&self, path: &Path, remote: &Remote) -> Result<(), GitError> {
        let argv = Self::lfs_args(path, "push", remote);
        self.run(
            git_span!("git.lfs_push", remote),
            "lfs_push",
            Some(remote),
            argv,
        )
        .await
        .map(|_| ())
    }

    async fn push(&self, path: &Path, remote: &Remote, prune: bool) -> Result<(), GitError> {
        let mut argv = Self::cache_args(path, ["push", "--force"]);
        if prune {
            argv.push("--prune".into());
        }
        argv.push((&remote.url).into());
        argv.extend(FETCH_REFSPECS.iter().map(OsString::from));
        self.run(git_span!("git.push", remote), "push", Some(remote), argv)
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_names() {
        assert_eq!(Side::Github.as_str(), "github");
        assert_eq!(Side::Forgejo.as_str(), "forgejo");
    }
}
