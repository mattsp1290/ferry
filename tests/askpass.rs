//! Askpass mode through the built binary, and real git reaching it.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferry::config::{Token, TokenFiles};
use ferry::git::{Git, GitErrorKind, GitRunner, GitSettings, Remote, Side};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const GITHUB_TOKEN: &str = "github-token-not-real";
const FORGEJO_TOKEN: &str = "test-token-not-real";

struct Env {
    _tmp: tempfile::TempDir,
    github_file: PathBuf,
    forgejo_file: PathBuf,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let github_file = tmp.path().join("github-token");
        let forgejo_file = tmp.path().join("forgejo-token");
        // Trailing whitespace must not reach git.
        std::fs::write(&github_file, format!("{GITHUB_TOKEN}\n\n")).unwrap();
        std::fs::write(&forgejo_file, format!("{FORGEJO_TOKEN} \t\n")).unwrap();
        Self {
            _tmp: tmp,
            github_file,
            forgejo_file,
        }
    }

    fn askpass(&self, prompt: &str) -> Output {
        self.askpass_with(prompt, &self.github_file, &self.forgejo_file)
    }

    fn askpass_with(&self, prompt: &str, github: &Path, forgejo: &Path) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ferry"))
            .arg(prompt)
            .env_clear()
            .env("FERRY_ASKPASS", "1")
            .env("FERRY_GITHUB_TOKEN_FILE", github)
            .env("FERRY_FORGEJO_TOKEN_FILE", forgejo)
            .env("FERRY_ASKPASS_GITHUB_HOST", "github.com")
            .env("FERRY_ASKPASS_FORGEJO_HOST", "forge.example:3000")
            .env("FERRY_ASKPASS_FORGEJO_USER", "ferry-bot")
            .output()
            .expect("binary runs")
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn github_username_and_password_prompts() {
    let env = Env::new();
    let output = env.askpass("Username for 'https://github.com': ");
    assert!(output.status.success());
    assert_eq!(stdout(&output), "x-access-token\n");

    let output = env.askpass("Password for 'https://x-access-token@github.com': ");
    assert!(output.status.success());
    assert_eq!(stdout(&output), format!("{GITHUB_TOKEN}\n"));
    assert!(output.stderr.is_empty());
}

#[test]
fn forgejo_username_and_password_prompts_with_port_and_path() {
    let env = Env::new();
    let output = env.askpass("Username for 'https://forge.example:3000/org/repo.git': ");
    assert!(output.status.success());
    assert_eq!(stdout(&output), "ferry-bot\n");

    let output = env.askpass("Password for 'https://ferry-bot@forge.example:3000': ");
    assert!(output.status.success());
    assert_eq!(stdout(&output), format!("{FORGEJO_TOKEN}\n"));
}

#[test]
fn unknown_host_exits_one_and_prints_nothing() {
    let env = Env::new();
    for prompt in [
        "Password for 'https://evil.example': ",
        "Password for 'https://forge.example': ",
        "Password for 'https://github.com.evil.example': ",
        "not a prompt",
    ] {
        let output = env.askpass(prompt);
        assert_eq!(output.status.code(), Some(1), "{prompt}");
        assert!(output.stdout.is_empty(), "{prompt}");
        assert!(output.stderr.is_empty(), "{prompt}");
    }
}

#[test]
fn missing_token_file_exits_one_and_prints_nothing() {
    let env = Env::new();
    let absent = env.github_file.with_file_name("absent");
    let output = env.askpass_with(
        "Password for 'https://github.com': ",
        &absent,
        &env.forgejo_file,
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty() && output.stderr.is_empty());

    let output = env.askpass_with(
        "Password for 'https://forge.example:3000': ",
        &env.github_file,
        &absent,
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
}

#[test]
fn no_prompt_argument_exits_one() {
    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .env_clear()
        .env("FERRY_ASKPASS", "1")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
}

// ------------------------------------------------------------ real git

fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Answers every request with `401` and `WWW-Authenticate: Basic`, recording
/// each `Authorization` header it sees.
async fn spawn_401_server() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let recorded = recorded.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut chunk = [0u8; 2048];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&chunk[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&request);
                for line in text.lines() {
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("authorization")
                    {
                        recorded.lock().unwrap().push(value.trim().to_string());
                    }
                }
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"x\"\r\n\
                          Content-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (port, seen)
}

fn runner_for(port: u16, github_side: bool, env: &Env, secrets: Vec<Token>) -> GitRunner {
    let host = format!("127.0.0.1:{port}");
    GitRunner::new(GitSettings {
        cache_dir: env._tmp.path().join("cache"),
        timeout: Duration::from_secs(30),
        kill_grace: Duration::from_millis(500),
        token_files: TokenFiles {
            github: Some(env.github_file.clone()),
            forgejo: Some(env.forgejo_file.clone()),
        },
        github_host: if github_side {
            host.clone()
        } else {
            "github.com".into()
        },
        forgejo_host: if github_side {
            "forge.invalid".into()
        } else {
            host
        },
        forgejo_user: "ferry".into(),
        secrets,
        askpass_path: env!("CARGO_BIN_EXE_ferry").into(),
        git_program: "git".into(),
        cancel: CancellationToken::new(),
    })
}

async fn real_git_sends_credentials(github_side: bool, user: &str, token: &str) {
    let env = Env::new();
    let (port, seen) = spawn_401_server().await;
    let runner = runner_for(port, github_side, &env, vec![Token::new(token)]);
    let remote = Remote {
        url: format!("http://127.0.0.1:{port}/org/repo.git"),
        side: if github_side {
            Side::Github
        } else {
            Side::Forgejo
        },
    };

    let error = runner
        .ls_remote(&remote)
        .await
        .expect_err("server always 401s");
    assert_eq!(error.kind, GitErrorKind::Auth, "{error}");
    assert!(!error.to_string().contains(token));

    let expected = format!("Basic {}", base64(format!("{user}:{token}").as_bytes()));
    let headers = seen.lock().unwrap().clone();
    assert!(
        headers.contains(&expected),
        "git never sent the expected credentials; saw {} header(s)",
        headers.len()
    );
}

#[tokio::test]
async fn real_git_gets_forgejo_credentials_through_askpass() {
    real_git_sends_credentials(false, "ferry", FORGEJO_TOKEN).await;
}

#[tokio::test]
async fn real_git_gets_github_credentials_through_askpass() {
    real_git_sends_credentials(true, "x-access-token", GITHUB_TOKEN).await;
}

#[test]
fn base64_helper_matches_known_vectors() {
    assert_eq!(base64(b"f"), "Zg==");
    assert_eq!(base64(b"fo"), "Zm8=");
    assert_eq!(base64(b"foo"), "Zm9v");
    assert_eq!(base64(b"ferry:x"), "ZmVycnk6eA==");
}

// ----------------------------------------------------------- redaction

#[tokio::test]
async fn stderr_containing_a_token_is_redacted_in_errors() {
    let env = Env::new();
    let script = env._tmp.path().join("fake-git");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho \"fatal: leaked {FORGEJO_TOKEN} here\" >&2\nexit 128\n"),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let runner = runner_for(1, false, &env, vec![Token::new(FORGEJO_TOKEN)]);
    let mut settings = runner.settings().clone();
    settings.git_program = script;
    let runner = GitRunner::new(settings);

    let remote = Remote {
        url: "https://forge.invalid/o/r.git".into(),
        side: Side::Forgejo,
    };
    let error = runner.ls_remote(&remote).await.expect_err("script fails");
    assert_eq!(error.exit_code, Some(128));
    assert!(error.stderr.contains("[redacted]"), "{}", error.stderr);
    assert!(!error.stderr.contains(FORGEJO_TOKEN));
    let shown = format!("{error} / {error:?}");
    assert!(!shown.contains(FORGEJO_TOKEN), "{shown}");
}
