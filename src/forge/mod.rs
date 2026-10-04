//! GitHub and Forgejo REST clients.
//!
//! Both clients share the HTTP client builder, the error type, and the request
//! plumbing in this module. Error values and span fields carry the route
//! template (for example `/repos/{owner}/{repo}`), the status code, and a short
//! capped message. They never carry a credential, a URL with a query, a header,
//! or a whole response body.

pub mod forgejo;
pub mod github;

use std::error::Error as _;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{RequestBuilder, StatusCode};
use serde::de::DeserializeOwned;
use thiserror::Error;
use tracing::{Instrument, Span};
use url::Url;

use crate::config::Token;
use crate::util::truncate_at_char_boundary;

pub use forgejo::{CreateOutcome, DestRepo, ForgejoClient, MARKER_TOPIC, RepoEdit};
pub use github::{GithubClient, SourceMeta};

/// Per-request timeout for both clients.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest body excerpt that may enter an error message.
const BODY_EXCERPT_MAX: usize = 512;

/// A failed forge API call. `Display` is safe to log.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ForgeError {
    #[error("{route}: not found")]
    NotFound { route: &'static str },
    #[error("{route}: authentication or authorization failed (HTTP {status}): {message}")]
    Auth {
        route: &'static str,
        status: u16,
        message: String,
    },
    #[error("{route}: rate limited (retry after {retry_after:?})")]
    RateLimited {
        route: &'static str,
        retry_after: Option<Duration>,
    },
    #[error("{route}: server error (HTTP {status}): {message}")]
    Server {
        route: &'static str,
        status: u16,
        message: String,
    },
    #[error("{route}: network error: {message}")]
    Network {
        route: &'static str,
        message: String,
    },
    #[error("{route}: unexpected response{}: {message}", status_suffix(*.status))]
    Unexpected {
        route: &'static str,
        status: Option<u16>,
        message: String,
    },
}

fn status_suffix(status: Option<u16>) -> String {
    status.map_or_else(String::new, |s| format!(" (HTTP {s})"))
}

/// The shared HTTP client: 30 s timeout and `User-Agent: ferry/<version>`.
pub fn http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent(concat!("ferry/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// A fully read response.
pub(crate) struct Raw {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

/// Build `base` + `segments`, percent-encoding each segment.
pub(crate) fn build_url(
    base: &str,
    segments: &[&str],
    route: &'static str,
) -> Result<Url, ForgeError> {
    let unexpected = |message: &str| ForgeError::Unexpected {
        route,
        status: None,
        message: message.to_owned(),
    };
    if segments
        .iter()
        .any(|segment| segment.is_empty() || *segment == "." || *segment == "..")
    {
        return Err(unexpected("invalid path segment"));
    }
    let mut url = Url::parse(base).map_err(|_| unexpected("invalid base URL"))?;
    url.path_segments_mut()
        .map_err(|()| unexpected("base URL cannot carry a path"))?
        .pop_if_empty()
        .extend(segments);
    Ok(url)
}

/// An `Authorization` header value that is marked sensitive.
pub(crate) fn auth_header(
    scheme: &str,
    token: &Token,
    route: &'static str,
) -> Result<HeaderValue, ForgeError> {
    let mut value =
        HeaderValue::from_str(&format!("{scheme} {}", token.expose())).map_err(|_| {
            ForgeError::Unexpected {
                route,
                status: None,
                message: "token is not a valid header value".to_owned(),
            }
        })?;
    value.set_sensitive(true);
    Ok(value)
}

/// Send `req` inside `span` and read the whole response.
pub(crate) async fn execute(
    req: RequestBuilder,
    route: &'static str,
    span: Span,
) -> Result<Raw, ForgeError> {
    let recorder = span.clone();
    async move {
        let response = req.send().await.map_err(|e| network_error(route, e))?;
        let status = response.status();
        recorder.record("http.status_code", status.as_u16());
        let headers = response.headers().clone();
        let body = response
            .bytes()
            .await
            .map_err(|e| network_error(route, e))?
            .to_vec();
        Ok(Raw {
            status,
            headers,
            body,
        })
    }
    .instrument(span)
    .await
}

fn network_error(route: &'static str, error: reqwest::Error) -> ForgeError {
    let error = error.without_url();
    let mut message = if error.is_timeout() {
        "timeout".to_owned()
    } else if error.is_connect() {
        "connect failed".to_owned()
    } else {
        "transport error".to_owned()
    };
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    truncate_at_char_boundary(&mut message, BODY_EXCERPT_MAX);
    ForgeError::Network { route, message }
}

/// A short, single-line excerpt of a response body.
fn excerpt(body: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(body)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    truncate_at_char_boundary(&mut text, BODY_EXCERPT_MAX);
    text
}

/// `Retry-After` in whole seconds. HTTP-date values are not interpreted.
pub(crate) fn retry_after_header(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Map a non-success response to an error with the rules shared by both clients.
pub(crate) fn map_status(raw: &Raw, route: &'static str) -> ForgeError {
    let status = raw.status.as_u16();
    match raw.status {
        StatusCode::NOT_FOUND => ForgeError::NotFound { route },
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ForgeError::Auth {
            route,
            status,
            message: excerpt(&raw.body),
        },
        StatusCode::TOO_MANY_REQUESTS => ForgeError::RateLimited {
            route,
            retry_after: retry_after_header(&raw.headers),
        },
        s if s.is_server_error() => ForgeError::Server {
            route,
            status,
            message: excerpt(&raw.body),
        },
        _ => ForgeError::Unexpected {
            route,
            status: Some(status),
            message: excerpt(&raw.body),
        },
    }
}

/// Require a 2xx status.
pub(crate) fn ensure_success(raw: Raw, route: &'static str) -> Result<Raw, ForgeError> {
    if raw.status.is_success() {
        Ok(raw)
    } else {
        Err(map_status(&raw, route))
    }
}

/// Deserialize a success body. A body that does not parse is `Unexpected`.
pub(crate) fn decode<T: DeserializeOwned>(raw: &Raw, route: &'static str) -> Result<T, ForgeError> {
    serde_json::from_slice(&raw.body).map_err(|e| ForgeError::Unexpected {
        route,
        status: Some(raw.status.as_u16()),
        // serde_json messages describe position and expected type, not content.
        message: format!("response body did not deserialize: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forge_modules_contain_no_delete_request() {
        let needles = [concat!("DEL", "ETE"), concat!(".del", "ete(")];
        let sources = [
            ("forgejo.rs", include_str!("forgejo.rs")),
            ("github.rs", include_str!("github.rs")),
            ("mod.rs", include_str!("mod.rs")),
        ];
        for (name, source) in sources {
            for needle in needles {
                assert!(!source.contains(needle), "{name} contains {needle:?}");
            }
        }
    }

    #[test]
    fn build_url_encodes_segments_and_keeps_base_path() {
        let url = build_url("http://h:1/prefix/", &["repos", "a/b", "c d"], "/r").unwrap();
        assert_eq!(url.as_str(), "http://h:1/prefix/repos/a%2Fb/c%20d");
        assert!(build_url("http://h", &["repos", ".."], "/r").is_err());
    }

    #[test]
    fn excerpt_is_capped() {
        let body = "é".repeat(1000);
        assert!(excerpt(body.as_bytes()).len() <= BODY_EXCERPT_MAX);
    }
}
