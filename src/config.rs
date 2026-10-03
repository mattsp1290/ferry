//! Configuration contract: one TOML file plus token-file paths in env.
//!
//! The file is read once at start. There is no hot reload and no env override
//! for individual keys.

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use url::Url;

/// Env variable with the path of the GitHub token file (optional).
pub const GITHUB_TOKEN_FILE_ENV: &str = "FERRY_GITHUB_TOKEN_FILE";
/// Env variable with the path of the Forgejo token file (required to sync).
pub const FORGEJO_TOKEN_FILE_ENV: &str = "FERRY_FORGEJO_TOKEN_FILE";
/// When set to `1`, `http` URLs pass validation. Tests use it.
pub const ALLOW_INSECURE_URLS_ENV: &str = "FERRY_ALLOW_INSECURE_URLS";

const MIN_POLL_INTERVAL_SECONDS: u64 = 30;
/// One day. Unbounded, the value would overflow the scheduler's clock
/// arithmetic, and a longer interval is not a mirror in any useful sense.
const MAX_POLL_INTERVAL_SECONDS: u64 = 86_400;
const MIN_METADATA_INTERVAL_SECONDS: u64 = 300;
const MAX_CONCURRENCY_RANGE: std::ops::RangeInclusive<usize> = 1..=8;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse config file {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error("invalid config: {}", .0.join("; "))]
    Invalid(Vec<String>),
    #[error("{0}")]
    Token(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub sync: SyncConfig,
    #[serde(default)]
    pub github: GithubConfig,
    pub forgejo: ForgejoConfig,
    #[serde(default)]
    pub health: HealthConfig,
    #[serde(default)]
    pub repos: Vec<RepoEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SyncConfig {
    pub poll_interval_seconds: u64,
    pub metadata_interval_seconds: u64,
    pub max_concurrency: usize,
    /// Applies to each git child process.
    pub git_timeout_seconds: u64,
    pub cache_dir: PathBuf,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            poll_interval_seconds: 300,
            metadata_interval_seconds: 3600,
            max_concurrency: 2,
            git_timeout_seconds: 1800,
            cache_dir: PathBuf::from("/var/lib/ferry/cache"),
        }
    }
}

impl SyncConfig {
    pub fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.poll_interval_seconds)
    }

    pub fn metadata_interval(&self) -> Duration {
        Duration::from_secs(self.metadata_interval_seconds)
    }

    pub fn git_timeout(&self) -> Duration {
        Duration::from_secs(self.git_timeout_seconds)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GithubConfig {
    pub api_url: String,
    pub git_url: String,
}

impl Default for GithubConfig {
    fn default() -> Self {
        Self {
            api_url: "https://api.github.com".to_string(),
            git_url: "https://github.com".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgejoConfig {
    /// Base URL for both the REST API and git transport.
    pub url: String,
    /// Basic-auth username that git pairs with the token.
    pub username: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HealthConfig {
    pub listen: String,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".to_string(),
        }
    }
}

/// One allowlist entry: a GitHub repository and its Forgejo destination.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoEntry {
    /// GitHub `owner/name`.
    pub github: String,
    /// Forgejo `owner/name`.
    pub forgejo: String,
    #[serde(default = "default_true")]
    pub lfs: bool,
    /// Whether the Actions unit is enabled on a repository ferry creates.
    #[serde(default)]
    pub actions: bool,
    /// Allows ferry to take over an existing, non-empty, unmarked repository.
    #[serde(default)]
    pub adopt: bool,
}

fn default_true() -> bool {
    true
}

impl RepoEntry {
    /// `(owner, name)` of the GitHub side. Valid after `Config::validate`.
    pub fn github_parts(&self) -> (&str, &str) {
        split_repo(&self.github)
    }

    /// `(owner, name)` of the Forgejo side. Valid after `Config::validate`.
    pub fn forgejo_parts(&self) -> (&str, &str) {
        split_repo(&self.forgejo)
    }

    /// The `repo` metric tag and span field: GitHub `owner/name` in lowercase.
    pub fn repo_tag(&self) -> String {
        self.github.to_lowercase()
    }
}

fn split_repo(value: &str) -> (&str, &str) {
    value.split_once('/').unwrap_or((value, ""))
}

impl Config {
    /// Reads and parses the file. Does not validate.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source: Box::new(source),
        })
    }

    /// Checks every rule and reports all violations, not only the first.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_with(insecure_urls_allowed())
    }

    /// `validate` with the insecure-URL switch passed in, so that tests do
    /// not have to mutate the process environment.
    pub fn validate_with(&self, allow_insecure_urls: bool) -> Result<(), ConfigError> {
        let mut violations = Vec::new();

        self.validate_sync(&mut violations);
        validate_url(
            "forgejo.url",
            &self.forgejo.url,
            allow_insecure_urls,
            &mut violations,
        );
        validate_url(
            "github.api_url",
            &self.github.api_url,
            allow_insecure_urls,
            &mut violations,
        );
        validate_url(
            "github.git_url",
            &self.github.git_url,
            allow_insecure_urls,
            &mut violations,
        );
        if self.forgejo.username.trim().is_empty() {
            violations.push("forgejo.username must not be empty".to_string());
        }
        if self.health.listen.parse::<SocketAddr>().is_err() {
            violations.push(format!(
                "health.listen {:?} is not a socket address such as 0.0.0.0:8080",
                self.health.listen
            ));
        }
        self.validate_repos(&mut violations);

        if violations.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(violations))
        }
    }

    fn validate_sync(&self, violations: &mut Vec<String>) {
        let sync = &self.sync;
        if !(MIN_POLL_INTERVAL_SECONDS..=MAX_POLL_INTERVAL_SECONDS)
            .contains(&sync.poll_interval_seconds)
        {
            violations.push(format!(
                "sync.poll_interval_seconds is {} but must be between {MIN_POLL_INTERVAL_SECONDS} and {MAX_POLL_INTERVAL_SECONDS}",
                sync.poll_interval_seconds
            ));
        }
        if sync.metadata_interval_seconds < MIN_METADATA_INTERVAL_SECONDS {
            violations.push(format!(
                "sync.metadata_interval_seconds is {} but must be at least {MIN_METADATA_INTERVAL_SECONDS}",
                sync.metadata_interval_seconds
            ));
        }
        if !MAX_CONCURRENCY_RANGE.contains(&sync.max_concurrency) {
            violations.push(format!(
                "sync.max_concurrency is {} but must be between {} and {}",
                sync.max_concurrency,
                MAX_CONCURRENCY_RANGE.start(),
                MAX_CONCURRENCY_RANGE.end()
            ));
        }
        if sync.git_timeout_seconds == 0 {
            violations.push("sync.git_timeout_seconds must be at least 1".to_string());
        }
        if !sync.cache_dir.is_absolute() {
            violations.push(format!(
                "sync.cache_dir {:?} must be an absolute path",
                sync.cache_dir
            ));
        }
    }

    fn validate_repos(&self, violations: &mut Vec<String>) {
        let mut github_seen: HashMap<String, usize> = HashMap::new();
        let mut forgejo_seen: HashMap<String, usize> = HashMap::new();

        for (index, entry) in self.repos.iter().enumerate() {
            let label = format!("repos[{index}]");
            if let Err(reason) = validate_repo_name(&entry.github) {
                violations.push(format!("{label}.github {:?} {reason}", entry.github));
            }
            if let Err(reason) = validate_repo_name(&entry.forgejo) {
                violations.push(format!("{label}.forgejo {:?} {reason}", entry.forgejo));
            }
            if let Some(first) = github_seen.insert(entry.github.to_lowercase(), index) {
                violations.push(format!(
                    "{label}.github {:?} duplicates repos[{first}].github",
                    entry.github
                ));
            }
            if let Some(first) = forgejo_seen.insert(entry.forgejo.to_lowercase(), index) {
                violations.push(format!(
                    "{label}.forgejo {:?} duplicates repos[{first}].forgejo",
                    entry.forgejo
                ));
            }
        }
    }

    /// The entries selected by `--repo` filters (GitHub `owner/name`,
    /// case-insensitive). An empty filter selects every entry. A filter that
    /// matches no entry is an error.
    pub fn select_repos(&self, filters: &[String]) -> Result<Vec<RepoEntry>, ConfigError> {
        if filters.is_empty() {
            return Ok(self.repos.clone());
        }
        let unknown: Vec<String> = filters
            .iter()
            .filter(|filter| {
                !self
                    .repos
                    .iter()
                    .any(|entry| entry.github.eq_ignore_ascii_case(filter))
            })
            .map(|filter| format!("--repo {filter:?} matches no [[repos]] entry"))
            .collect();
        if !unknown.is_empty() {
            return Err(ConfigError::Invalid(unknown));
        }
        Ok(self
            .repos
            .iter()
            .filter(|entry| {
                filters
                    .iter()
                    .any(|filter| entry.github.eq_ignore_ascii_case(filter))
            })
            .cloned()
            .collect())
    }
}

fn insecure_urls_allowed() -> bool {
    std::env::var(ALLOW_INSECURE_URLS_ENV).is_ok_and(|value| value == "1")
}

/// Rule 1: `owner/name`, each segment `[A-Za-z0-9_.-]+`, no `.`/`..`
/// segment, no `.git` suffix.
fn validate_repo_name(value: &str) -> Result<(), &'static str> {
    let Some((owner, name)) = value.split_once('/') else {
        return Err("must have the form owner/name");
    };
    for segment in [owner, name] {
        if segment.is_empty()
            || !segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        {
            return Err("must have the form owner/name using only letters, digits, '_', '.', '-'");
        }
        if segment == "." || segment == ".." {
            return Err("must not use '.' or '..' as a segment");
        }
    }
    if name.to_ascii_lowercase().ends_with(".git") {
        return Err("must not end in .git");
    }
    Ok(())
}

fn validate_url(field: &str, value: &str, allow_insecure: bool, violations: &mut Vec<String>) {
    let url = match Url::parse(value) {
        Ok(url) => url,
        Err(error) => {
            violations.push(format!("{field} {value:?} is not a URL: {error}"));
            return;
        }
    };
    match url.scheme() {
        "https" => {}
        "http" if allow_insecure => {}
        "http" => violations.push(format!(
            "{field} must use https (http is accepted only with {ALLOW_INSECURE_URLS_ENV}=1)"
        )),
        other => violations.push(format!("{field} must use https, not {other}")),
    }
    // Never echo the value here: userinfo is where a credential would sit.
    if !url.username().is_empty() || url.password().is_some() {
        violations.push(format!("{field} must not contain userinfo"));
    }
    if url.host_str().is_none_or(str::is_empty) {
        violations.push(format!("{field} must name a host"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        violations.push(format!("{field} must not contain a query or fragment"));
    }
}

/// A credential. `Debug` and `Display` never print the value.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret value. Callers must not log it or place it in argv, a URL,
    /// a span attribute, or a metric tag.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token([redacted])")
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// Token file paths, taken from env only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenFiles {
    pub github: Option<PathBuf>,
    pub forgejo: Option<PathBuf>,
}

impl TokenFiles {
    pub fn from_env() -> Self {
        let path = |name: &str| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        Self {
            github: path(GITHUB_TOKEN_FILE_ENV),
            forgejo: path(FORGEJO_TOKEN_FILE_ENV),
        }
    }

    pub fn load(&self) -> Result<Tokens, ConfigError> {
        Ok(Tokens {
            github: load_github_token(self.github.as_deref()),
            forgejo: load_forgejo_token(self.forgejo.as_deref())?,
        })
    }
}

/// The loaded credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokens {
    pub github: Option<Token>,
    pub forgejo: Token,
}

impl Tokens {
    /// Every loaded secret value, for redaction.
    pub fn secrets(&self) -> Vec<Token> {
        self.github
            .iter()
            .chain(std::iter::once(&self.forgejo))
            .cloned()
            .collect()
    }
}

/// Reads a token file and trims trailing whitespace. `None` when the file is
/// missing, unreadable, or empty.
pub fn read_token_file(path: &Path) -> Option<Token> {
    let text = std::fs::read_to_string(path).ok()?;
    let value = text.trim_end();
    (!value.is_empty()).then(|| Token::new(value))
}

/// An unset variable, a missing file, and an empty file all mean "no GitHub
/// token": ferry then talks to GitHub unauthenticated.
pub fn load_github_token(path: Option<&Path>) -> Option<Token> {
    path.and_then(read_token_file)
}

/// The Forgejo token is required for `run` and `sync`.
pub fn load_forgejo_token(path: Option<&Path>) -> Result<Token, ConfigError> {
    let path = path.ok_or_else(|| {
        ConfigError::Token(format!(
            "{FORGEJO_TOKEN_FILE_ENV} is not set; it must name the Forgejo token file"
        ))
    })?;
    read_token_file(path).ok_or_else(|| {
        ConfigError::Token(format!(
            "Forgejo token file {} ({FORGEJO_TOKEN_FILE_ENV}) is missing, unreadable, or empty",
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_never_prints_its_value() {
        let token = Token::new("s3cr3t-value");
        assert!(!format!("{token:?}").contains("s3cr3t-value"));
        assert!(!format!("{token}").contains("s3cr3t-value"));
        let tokens = Tokens {
            github: Some(token.clone()),
            forgejo: token,
        };
        assert!(!format!("{tokens:?}").contains("s3cr3t-value"));
    }

    #[test]
    fn repo_parts_and_tag() {
        let entry = RepoEntry {
            github: "Owner/Repo.Name".to_string(),
            forgejo: "dest/repo".to_string(),
            lfs: true,
            actions: false,
            adopt: false,
        };
        assert_eq!(entry.github_parts(), ("Owner", "Repo.Name"));
        assert_eq!(entry.forgejo_parts(), ("dest", "repo"));
        assert_eq!(entry.repo_tag(), "owner/repo.name");
    }
}
