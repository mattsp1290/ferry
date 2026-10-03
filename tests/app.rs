//! The `run` and `sync --once` commands through the real binary: startup
//! checks, exit codes, health endpoints, and shutdown.
//!
//! The sync algorithm itself is covered in `sync_flow.rs`. These tests need a
//! Forgejo API only for the startup check, so a small mock is enough.

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::process::{Child, Command};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod support;

use support::{FORGEJO_TOKEN, stdout};

const SPAWN_ATTEMPTS: u32 = 5;

/// A free loopback port that no other test in this process has been given.
fn free_port() -> u16 {
    static HANDED_OUT: Mutex<Vec<u16>> = Mutex::new(Vec::new());
    loop {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        let mut handed_out = HANDED_OUT.lock().unwrap_or_else(PoisonError::into_inner);
        if !handed_out.contains(&port) {
            handed_out.push(port);
            return port;
        }
    }
}

struct Fixture {
    dir: TempDir,
    forgejo: MockServer,
}

impl Fixture {
    /// A fake Forgejo that answers `GET /api/v1/user` with `whoami_status`.
    async fn new(whoami_status: u16) -> Self {
        let forgejo = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/user"))
            .respond_with(
                ResponseTemplate::new(whoami_status).set_body_json(json!({"login": "ferry"})),
            )
            .mount(&forgejo)
            .await;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("forgejo-token"),
            format!("{FORGEJO_TOKEN}\n"),
        )
        .expect("write token");
        Self { dir, forgejo }
    }

    fn token_file(&self) -> PathBuf {
        self.dir.path().join("forgejo-token")
    }

    /// Writes a config with the given extra TOML and returns its path.
    fn config(&self, extra: &str) -> PathBuf {
        self.config_with_cache(&self.dir.path().join("cache"), extra)
    }

    /// Like `config`, with an explicit `sync.cache_dir`.
    fn config_with_cache(&self, cache_dir: &Path, extra: &str) -> PathBuf {
        let path = self.dir.path().join("ferry.toml");
        let text = format!(
            "[sync]\ncache_dir = \"{}\"\n\n[forgejo]\nurl = \"{}\"\nusername = \"ferry\"\n\n{extra}",
            cache_dir.display(),
            self.forgejo.uri(),
        );
        std::fs::write(&path, text).expect("write config");
        path
    }

    /// The binary with a clean ferry and Datadog environment.
    fn ferry(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ferry"));
        for name in [
            "FERRY_CONFIG",
            "FERRY_ASKPASS",
            "FERRY_GITHUB_TOKEN_FILE",
            "FERRY_LOG_FORMAT",
            "FERRY_LOG_LEVEL",
            "DD_DOGSTATSD_URL",
            "DD_TRACE_AGENT_URL",
            "DD_SERVICE",
            "DD_ENV",
            "DD_VERSION",
        ] {
            command.env_remove(name);
        }
        command
            .env("FERRY_ALLOW_INSECURE_URLS", "1")
            .env("FERRY_FORGEJO_TOKEN_FILE", self.token_file())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    /// Starts `ferry run` on a free port and returns the child and the base
    /// URL once `/healthz` answers 200.
    ///
    /// A port that was bound and released can be lost to another process
    /// before ferry binds it, and then ferry exits 1. Only that early exit,
    /// before the health endpoint first answers, is retried (new port, new
    /// config, up to `SPAWN_ATTEMPTS`); everything after is the caller's to
    /// assert strictly.
    async fn spawn_run(&self) -> (Child, String) {
        for attempt in 1..=SPAWN_ATTEMPTS {
            let port = free_port();
            let config = self.config(&format!("[health]\nlisten = \"127.0.0.1:{port}\"\n"));
            let mut child = self
                .ferry()
                .args(["run", "--config"])
                .arg(&config)
                .spawn()
                .expect("ferry starts");
            let base = format!("http://127.0.0.1:{port}");

            let deadline = Instant::now() + Duration::from_secs(30);
            let exited = loop {
                let live = get_status(&format!("{base}/healthz")).await == Some(200);
                // Another process may answer on a port our ferry failed to
                // bind, so a 200 only counts while the child is still alive.
                if let Some(status) = child.try_wait().expect("try_wait") {
                    break status;
                }
                if live {
                    return (child, base);
                }
                assert!(Instant::now() < deadline, "ferry never became live");
                tokio::time::sleep(Duration::from_millis(50)).await;
            };
            eprintln!("ferry exited early on attempt {attempt} ({exited}); retrying on a new port");
        }
        panic!("ferry exited early on all {SPAWN_ATTEMPTS} attempts");
    }

    async fn sync_once(&self, config: &Path, extra_args: &[&str]) -> Output {
        self.ferry()
            .args(["sync", "--once", "--config"])
            .arg(config)
            .args(extra_args)
            .output()
            .await
            .expect("ferry runs")
    }

    async fn whoami_requests(&self) -> usize {
        self.forgejo
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|request| request.url.path() == "/api/v1/user")
            .count()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Every stdout line must be one JSON object: the Agent parses them.
fn json_lines(output: &Output) -> Vec<Value> {
    stdout(output)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|_| panic!("not JSON: {line}")))
        .collect()
}

fn assert_no_token(output: &Output) {
    assert!(!stdout(output).contains(FORGEJO_TOKEN), "token on stdout");
    assert!(!stderr(output).contains(FORGEJO_TOKEN), "token on stderr");
}

#[tokio::test]
async fn sync_once_with_an_empty_allowlist_exits_0_without_datadog() {
    let fixture = Fixture::new(200).await;
    let config = fixture.config("");

    let output = fixture.sync_once(&config, &[]).await;

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    // Startup check 4: the Forgejo token was tried once.
    assert_eq!(fixture.whoami_requests().await, 1);
    let lines = json_lines(&output);
    let finished = lines
        .iter()
        .find(|line| line["message"] == "sync pass finished")
        .expect("a summary line");
    assert_eq!(finished["service"], "ferry");
    assert_eq!(finished["failed"], 0);
    assert!(fixture.dir.path().join("cache").is_dir());
    assert_no_token(&output);
}

#[tokio::test]
async fn sync_once_without_the_forgejo_token_file_exits_2() {
    let fixture = Fixture::new(200).await;
    let config = fixture.config("");

    let output = fixture
        .ferry()
        .env_remove("FERRY_FORGEJO_TOKEN_FILE")
        .args(["sync", "--once", "--config"])
        .arg(&config)
        .output()
        .await
        .expect("ferry runs");

    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("FERRY_FORGEJO_TOKEN_FILE"));
    assert_eq!(fixture.whoami_requests().await, 0);

    // An empty token file is the same configuration error.
    std::fs::write(fixture.token_file(), "\n").expect("empty token");
    let output = fixture.sync_once(&config, &[]).await;
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
}

#[tokio::test]
async fn sync_once_exits_1_when_forgejo_rejects_the_token() {
    let fixture = Fixture::new(401).await;
    let config = fixture.config("");

    let output = fixture.sync_once(&config, &[]).await;

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("Forgejo is not reachable"));
    assert_no_token(&output);
}

#[tokio::test]
async fn sync_once_exits_1_when_the_cache_directory_is_unusable() {
    let fixture = Fixture::new(200).await;
    let config = fixture.config_with_cache(Path::new("/dev/null/cache"), "");

    let output = fixture.sync_once(&config, &[]).await;

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("cache directory"));
}

#[tokio::test]
async fn sync_once_exits_2_for_an_invalid_config_or_unknown_repo_filter() {
    let fixture = Fixture::new(200).await;
    let config = fixture.config("[[repos]]\ngithub = \"owner/alpha\"\nforgejo = \"ferry/alpha\"\n");

    let output = fixture.sync_once(&config, &["--repo", "owner/other"]).await;
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("owner/other"));

    let bad = fixture.config("[sync2]\n");
    let output = fixture.sync_once(&bad, &[]).await;
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert_eq!(fixture.whoami_requests().await, 0);
}

#[tokio::test]
async fn sync_once_exits_1_when_an_entry_fails() {
    let fixture = Fixture::new(200).await;
    // The discard port: nothing listens there, and unlike a port that was
    // bound and released, a sibling test cannot take it.
    let closed_port = 9;
    let config = fixture.config(&format!(
        "[github]\ngit_url = \"http://127.0.0.1:{closed_port}\"\n\n\
         [[repos]]\ngithub = \"owner/alpha\"\nforgejo = \"ferry/alpha\"\nlfs = false\n"
    ));

    let output = fixture.sync_once(&config, &["--repo", "Owner/Alpha"]).await;

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let lines = json_lines(&output);
    let failed = lines
        .iter()
        .find(|line| line["message"] == "sync failed")
        .expect("an error line for the entry");
    assert_eq!(failed["level"], "error");
    assert_eq!(failed["repo"], "owner/alpha");
    assert_eq!(failed["error_kind"], "network");
    let summary = lines
        .iter()
        .find(|line| line["message"] == "sync pass finished")
        .expect("a summary line");
    assert_eq!(summary["failed"], 1);
    assert_no_token(&output);
}

async fn get_status(url: &str) -> Option<u16> {
    reqwest::get(url)
        .await
        .ok()
        .map(|response| response.status().as_u16())
}

/// Sends SIGTERM and expects a clean exit within the shutdown grace.
async fn terminate_and_expect_exit_0(child: &mut Child) {
    let pid = i32::try_from(child.id().expect("pid")).expect("pid fits");
    kill(Pid::from_raw(pid), Signal::SIGTERM).expect("send SIGTERM");
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("ferry exits after SIGTERM")
        .expect("wait");
    assert_eq!(status.code(), Some(0));
}

#[tokio::test]
async fn run_serves_health_and_exits_0_on_sigterm() {
    let fixture = Fixture::new(200).await;
    let (mut child, base) = fixture.spawn_run().await;

    // Ready means: startup checks passed, including one Forgejo API call.
    let deadline = Instant::now() + Duration::from_secs(30);
    while get_status(&format!("{base}/readyz")).await != Some(200) {
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "ferry exited early"
        );
        assert!(Instant::now() < deadline, "ferry never became ready");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(get_status(&format!("{base}/healthz")).await, Some(200));
    assert_eq!(get_status(&format!("{base}/metrics")).await, Some(404));
    assert!(fixture.whoami_requests().await >= 1);

    terminate_and_expect_exit_0(&mut child).await;
}

#[tokio::test]
async fn run_stays_up_and_not_ready_while_forgejo_rejects_the_token() {
    let fixture = Fixture::new(503).await;
    let (mut child, base) = fixture.spawn_run().await;

    // A Forgejo outage must leave the pod running and not ready, so that it
    // does not crash-loop.
    let deadline = Instant::now() + Duration::from_secs(30);
    while fixture.whoami_requests().await == 0 {
        assert!(Instant::now() < deadline, "the token was never tried");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(get_status(&format!("{base}/readyz")).await, Some(503));
    assert_eq!(get_status(&format!("{base}/healthz")).await, Some(200));
    assert!(child.try_wait().expect("try_wait").is_none());

    terminate_and_expect_exit_0(&mut child).await;
}
