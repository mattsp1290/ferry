//! Askpass mode: git runs the ferry binary itself to obtain credentials.
//!
//! Git (and git-lfs, through `git credential fill`) call the program with one
//! prompt argument such as `Username for 'https://github.com': `. The answer
//! goes to stdout. This mode prints nothing else and logs nothing.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use url::Url;

use crate::config::{FORGEJO_TOKEN_FILE_ENV, GITHUB_TOKEN_FILE_ENV, read_token_file};

/// Env variable that switches the binary into askpass mode when set to `1`.
pub const ASKPASS_ENV: &str = "FERRY_ASKPASS";
/// Host (`host[:port]`) whose credentials are the GitHub token.
pub const GITHUB_HOST_ENV: &str = "FERRY_ASKPASS_GITHUB_HOST";
/// Host (`host[:port]`) whose credentials are the Forgejo token.
pub const FORGEJO_HOST_ENV: &str = "FERRY_ASKPASS_FORGEJO_HOST";
/// URL schemes whose prompts may receive credentials.
pub const GITHUB_SCHEME_ENV: &str = "FERRY_ASKPASS_GITHUB_SCHEME";
pub const FORGEJO_SCHEME_ENV: &str = "FERRY_ASKPASS_FORGEJO_SCHEME";
/// Basic-auth username for Forgejo.
pub const FORGEJO_USER_ENV: &str = "FERRY_ASKPASS_FORGEJO_USER";

/// The username GitHub accepts together with a token.
const GITHUB_USERNAME: &str = "x-access-token";

/// Whether the process was started by git as its askpass helper.
pub fn requested() -> bool {
    std::env::var(ASKPASS_ENV).is_ok_and(|value| value == "1")
}

/// Lowercase `host[:port]` of a URL. The port is present only when the URL
/// carries an explicit non-default one.
pub fn host_port(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    Some(match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

/// What askpass mode reads from its environment.
#[derive(Debug, Clone, Default)]
pub struct AskpassEnv {
    pub github_host: Option<String>,
    pub github_scheme: Option<String>,
    pub forgejo_host: Option<String>,
    pub forgejo_scheme: Option<String>,
    pub forgejo_user: Option<String>,
    pub github_token_file: Option<PathBuf>,
    pub forgejo_token_file: Option<PathBuf>,
}

impl AskpassEnv {
    pub fn from_env() -> Self {
        let text = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        let path = |name: &str| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        Self {
            github_host: text(GITHUB_HOST_ENV),
            github_scheme: Some(text(GITHUB_SCHEME_ENV).unwrap_or_else(|| "https".into())),
            forgejo_host: text(FORGEJO_HOST_ENV),
            forgejo_scheme: Some(text(FORGEJO_SCHEME_ENV).unwrap_or_else(|| "https".into())),
            forgejo_user: text(FORGEJO_USER_ENV),
            github_token_file: path(GITHUB_TOKEN_FILE_ENV),
            forgejo_token_file: path(FORGEJO_TOKEN_FILE_ENV),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptKind {
    Username,
    Password,
}

/// Splits a prompt into its kind and the host of the quoted URL.
fn parse_prompt(prompt: &str) -> Option<(PromptKind, String, String)> {
    let kind = if prompt.starts_with("Username for") {
        PromptKind::Username
    } else if prompt.starts_with("Password for") {
        PromptKind::Password
    } else {
        return None;
    };
    let start = prompt.find('\'')? + 1;
    let end = prompt.rfind('\'')?;
    if end < start {
        return None;
    }
    let url = &prompt[start..end];
    let scheme = Url::parse(url).ok()?.scheme().to_owned();
    Some((kind, host_port(url)?, scheme))
}

/// The answer to `prompt`, or `None` when ferry has no credential for it.
pub fn answer(prompt: &str, env: &AskpassEnv) -> Option<String> {
    let (kind, host, scheme) = parse_prompt(prompt)?;
    let host_matches = |configured: &Option<String>| {
        configured
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case(&host))
    };
    let (username, token_file) = if host_matches(&env.github_host)
        && env.github_scheme.as_deref() == Some(&scheme)
    {
        (GITHUB_USERNAME.to_string(), env.github_token_file.as_ref())
    } else if host_matches(&env.forgejo_host) && env.forgejo_scheme.as_deref() == Some(&scheme) {
        (env.forgejo_user.clone()?, env.forgejo_token_file.as_ref())
    } else {
        return None;
    };
    let token = read_token_file(token_file?)?;
    Some(match kind {
        PromptKind::Username => username,
        PromptKind::Password => token.expose().to_string(),
    })
}

/// Runs askpass mode. `prompt` is git's prompt, taken from `argv[1]`.
pub fn run(prompt: Option<&str>) -> ExitCode {
    let Some(value) = prompt.and_then(|prompt| answer(prompt, &AskpassEnv::from_env())) else {
        return ExitCode::from(1);
    };
    let mut stdout = std::io::stdout().lock();
    if writeln!(stdout, "{value}")
        .and_then(|()| stdout.flush())
        .is_err()
    {
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with_tokens(dir: &std::path::Path) -> AskpassEnv {
        let gh = dir.join("gh");
        let fj = dir.join("fj");
        std::fs::write(&gh, "gh-token-not-real\n\n").unwrap();
        std::fs::write(&fj, "fj-token-not-real  \n").unwrap();
        AskpassEnv {
            github_host: Some("github.com".into()),
            github_scheme: Some("https".into()),
            forgejo_host: Some("127.0.0.1:3000".into()),
            forgejo_scheme: Some("http".into()),
            forgejo_user: Some("ferry".into()),
            github_token_file: Some(gh),
            forgejo_token_file: Some(fj),
        }
    }

    #[test]
    fn host_port_handles_userinfo_path_and_ports() {
        assert_eq!(
            host_port("https://github.com").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            host_port("https://x-access-token@GitHub.com/o/r.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            host_port("http://u:p@127.0.0.1:3000/a/b").as_deref(),
            Some("127.0.0.1:3000")
        );
        assert_eq!(host_port("https://forge:443/").as_deref(), Some("forge"));
        assert_eq!(host_port("not a url"), None);
    }

    #[test]
    fn answers_github_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_with_tokens(dir.path());
        assert_eq!(
            answer("Username for 'https://github.com': ", &env).as_deref(),
            Some("x-access-token")
        );
        assert_eq!(
            answer("Password for 'https://x-access-token@github.com': ", &env).as_deref(),
            Some("gh-token-not-real")
        );
    }

    #[test]
    fn answers_forgejo_prompts_with_port_and_path() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_with_tokens(dir.path());
        assert_eq!(
            answer("Username for 'http://127.0.0.1:3000/org/repo.git': ", &env).as_deref(),
            Some("ferry")
        );
        assert_eq!(
            answer("Password for 'http://ferry@127.0.0.1:3000': ", &env).as_deref(),
            Some("fj-token-not-real")
        );
    }

    #[test]
    fn rejects_unknown_host_port_mismatch_and_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_with_tokens(dir.path());
        for prompt in [
            "Username for 'https://evil.example': ",
            "Password for 'http://github.com': ",
            "Password for 'https://127.0.0.1:3000': ",
            "Password for 'http://127.0.0.1:3001': ",
            "Password for 'http://127.0.0.1': ",
            "Password for 'https://github.com.evil.example': ",
            "Enter passphrase for key: ",
            "Username for https://github.com: ",
            "Username for '': ",
            "",
        ] {
            assert_eq!(answer(prompt, &env), None, "{prompt:?}");
        }
    }

    #[test]
    fn missing_token_or_user_or_file_gives_no_answer() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = env_with_tokens(dir.path());
        env.github_token_file = None;
        assert_eq!(answer("Username for 'https://github.com': ", &env), None);
        env.forgejo_token_file = Some(dir.path().join("absent"));
        assert_eq!(answer("Password for 'http://127.0.0.1:3000': ", &env), None);
        let mut env = env_with_tokens(dir.path());
        env.forgejo_user = None;
        assert_eq!(answer("Username for 'http://127.0.0.1:3000': ", &env), None);
    }
}
