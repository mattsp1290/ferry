//! Read-only GitHub client. Ferry never writes to GitHub.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{ACCEPT, HeaderMap};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use tracing::field::Empty;

use super::{
    ForgeError, Raw, auth_header, authorized, build_url, decode, ensure_success, execute,
    retry_after_header,
};
use crate::config::Token;

const ROUTE_REPO: &str = "/repos/{owner}/{repo}";

/// The source repository facts ferry needs. The default branch comes from git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceMeta {
    pub description: String,
    pub private: bool,
    pub archived: bool,
}

#[derive(Deserialize)]
struct WireRepo {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    private: bool,
    #[serde(default)]
    archived: bool,
}

#[derive(Debug, Clone)]
pub struct GithubClient {
    http: reqwest::Client,
    base: String,
    token: Option<Token>,
}

impl GithubClient {
    pub fn new(http: reqwest::Client, api_url: &str, token: Option<Token>) -> Self {
        Self {
            http,
            base: api_url.trim_end_matches('/').to_owned(),
            token,
        }
    }

    /// `GET /repos/{owner}/{repo}`.
    pub async fn get_repo(&self, owner: &str, name: &str) -> Result<SourceMeta, ForgeError> {
        let url = build_url(&self.base, &["repos", owner, name], ROUTE_REPO)?;
        let mut req = self
            .http
            .request(Method::GET, url)
            .header(ACCEPT, "application/vnd.github+json");
        if let Some(token) = &self.token {
            req = authorized(req, auth_header("Bearer", token, ROUTE_REPO)?);
        }
        let span = tracing::info_span!(
            "github.api",
            http.method = "GET",
            http.route = ROUTE_REPO,
            http.status_code = Empty
        );
        let raw = execute(req, ROUTE_REPO, span).await?;
        if let Some(limited) = rate_limit(&raw) {
            return Err(limited);
        }
        let raw = ensure_success(raw, ROUTE_REPO)?;
        let wire: WireRepo = decode(&raw, ROUTE_REPO)?;
        Ok(SourceMeta {
            description: wire.description.unwrap_or_default(),
            private: wire.private,
            archived: wire.archived,
        })
    }
}

/// GitHub signals rate limiting with 403 or 429 plus headers. A 429 is always
/// a rate limit.
fn rate_limit(raw: &Raw) -> Option<ForgeError> {
    if raw.status != StatusCode::FORBIDDEN && raw.status != StatusCode::TOO_MANY_REQUESTS {
        return None;
    }
    let exhausted = header_str(&raw.headers, "x-ratelimit-remaining").is_some_and(|v| v == "0");
    let has_retry_after = raw.headers.contains_key("retry-after");
    if !(exhausted || has_retry_after || raw.status == StatusCode::TOO_MANY_REQUESTS) {
        return None;
    }
    Some(ForgeError::RateLimited {
        route: ROUTE_REPO,
        retry_after: retry_after_header(&raw.headers).or_else(|| reset_delay(&raw.headers)),
    })
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok().map(str::trim)
}

/// `x-ratelimit-reset` (epoch seconds) minus now.
fn reset_delay(headers: &HeaderMap) -> Option<Duration> {
    let reset: u64 = header_str(headers, "x-ratelimit-reset")?.parse().ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(Duration::from_secs(reset.saturating_sub(now)))
}
