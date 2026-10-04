//! Forgejo client: provision, mark, and reconcile destination repositories.
//!
//! There is no method here that removes a repository or a topic.

use reqwest::header::AUTHORIZATION;
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Deserializer, Serialize};
use tokio::sync::OnceCell;
use tracing::field::Empty;

use super::{ForgeError, Raw, auth_header, build_url, decode, ensure_success, execute};
use crate::config::Token;

/// Topic that marks a repository as managed by ferry.
pub const MARKER_TOPIC: &str = "ferry-mirror";

const ROUTE_REPO: &str = "/api/v1/repos/{owner}/{repo}";
const ROUTE_USER: &str = "/api/v1/user";
const ROUTE_USER_REPOS: &str = "/api/v1/user/repos";
const ROUTE_ORG_REPOS: &str = "/api/v1/orgs/{org}/repos";
const ROUTE_TOPICS: &str = "/api/v1/repos/{owner}/{repo}/topics";
const ROUTE_TOPIC: &str = "/api/v1/repos/{owner}/{repo}/topics/{topic}";

/// Page size for the topics listing. A repository has far fewer topics.
const TOPICS_LIMIT: &str = "100";

fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// The destination repository facts ferry reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct DestRepo {
    #[serde(default, deserialize_with = "null_default")]
    pub default_branch: String,
    #[serde(default, deserialize_with = "null_default")]
    pub description: String,
    #[serde(default, deserialize_with = "null_default")]
    pub private: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub mirror: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub empty: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub has_actions: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub topics: Vec<String>,
}

/// Whether `create_repo` made the repository or found it already present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateOutcome {
    Created,
    AlreadyExisted,
}

/// Fields to change. Only `Some` fields are sent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RepoEdit {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_actions: Option<bool>,
}

impl RepoEdit {
    pub fn is_empty(&self) -> bool {
        self.description.is_none() && self.default_branch.is_none() && self.has_actions.is_none()
    }
}

#[derive(Serialize)]
struct CreateBody<'a> {
    name: &'a str,
    private: bool,
    auto_init: bool,
    description: &'a str,
}

#[derive(Deserialize)]
struct WireUser {
    #[serde(default)]
    login: String,
}

#[derive(Deserialize)]
struct WireTopics {
    #[serde(default, deserialize_with = "null_default")]
    topics: Vec<String>,
}

#[derive(Debug)]
pub struct ForgejoClient {
    http: reqwest::Client,
    base: String,
    token: Token,
    login: OnceCell<String>,
}

impl ForgejoClient {
    pub fn new(http: reqwest::Client, url: &str, token: Token) -> Self {
        Self {
            http,
            base: url.trim_end_matches('/').to_owned(),
            token,
            login: OnceCell::new(),
        }
    }

    /// Send a bodyless request inside a `forgejo.api` span.
    async fn get(
        &self,
        route: &'static str,
        segments: &[&str],
        query: &[(&str, &str)],
    ) -> Result<Raw, ForgeError> {
        self.request(Method::GET, route, segments, query, None::<&()>)
            .await
    }

    async fn send_json<T: Serialize + ?Sized>(
        &self,
        method: Method,
        route: &'static str,
        segments: &[&str],
        body: Option<&T>,
    ) -> Result<Raw, ForgeError> {
        self.request(method, route, segments, &[], body).await
    }

    async fn request<T: Serialize + ?Sized>(
        &self,
        method: Method,
        route: &'static str,
        segments: &[&str],
        query: &[(&str, &str)],
        body: Option<&T>,
    ) -> Result<Raw, ForgeError> {
        let mut url = build_url(&self.base, segments, route)?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        let span = tracing::info_span!(
            "forgejo.api",
            http.method = %method,
            http.route = route,
            http.status_code = Empty
        );
        let mut request = self
            .http
            .request(method, url)
            .header(AUTHORIZATION, auth_header("token", &self.token, route)?);
        if let Some(body) = body {
            request = request.json(body);
        }
        execute(request, route, span).await
    }

    /// `GET /api/v1/repos/{owner}/{repo}`. A missing repository is `Ok(None)`.
    pub async fn get_repo(&self, owner: &str, name: &str) -> Result<Option<DestRepo>, ForgeError> {
        let raw = self
            .get(ROUTE_REPO, &["api", "v1", "repos", owner, name], &[])
            .await?;
        if raw.status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let raw = ensure_success(raw, ROUTE_REPO)?;
        decode(&raw, ROUTE_REPO).map(Some)
    }

    /// `GET /api/v1/user`: the token's login. Cached after the first success.
    pub async fn whoami(&self) -> Result<String, ForgeError> {
        self.login
            .get_or_try_init(|| async {
                let raw = self.get(ROUTE_USER, &["api", "v1", "user"], &[]).await?;
                let raw = ensure_success(raw, ROUTE_USER)?;
                let user: WireUser = decode(&raw, ROUTE_USER)?;
                if user.login.is_empty() {
                    return Err(ForgeError::Unexpected {
                        route: ROUTE_USER,
                        status: Some(raw.status.as_u16()),
                        message: "response carries no login".to_owned(),
                    });
                }
                Ok(user.login)
            })
            .await
            .cloned()
    }

    /// Create a private, empty repository. The user endpoint is used when
    /// `owner` is the token's login, the organization endpoint otherwise.
    /// HTTP 409 means the repository exists: it is read back and returned with
    /// [`CreateOutcome::AlreadyExisted`].
    pub async fn create_repo(
        &self,
        owner: &str,
        name: &str,
        description: &str,
    ) -> Result<(DestRepo, CreateOutcome), ForgeError> {
        let login = self.whoami().await?;
        let body = CreateBody {
            name,
            private: true,
            auto_init: false,
            description,
        };
        let (route, segments) = if owner.eq_ignore_ascii_case(&login) {
            (ROUTE_USER_REPOS, vec!["api", "v1", "user", "repos"])
        } else {
            (ROUTE_ORG_REPOS, vec!["api", "v1", "orgs", owner, "repos"])
        };
        let raw = self
            .send_json(Method::POST, route, &segments, Some(&body))
            .await?;
        if raw.status == StatusCode::CONFLICT {
            return match self.get_repo(owner, name).await? {
                Some(repo) => Ok((repo, CreateOutcome::AlreadyExisted)),
                None => Err(ForgeError::Unexpected {
                    route,
                    status: Some(409),
                    message: "create reported a conflict but the repository is absent".to_owned(),
                }),
            };
        }
        let raw = ensure_success(raw, route)?;
        Ok((decode(&raw, route)?, CreateOutcome::Created))
    }

    /// `PATCH /api/v1/repos/{owner}/{repo}` with only the `Some` fields. Sends
    /// nothing when `edit` is empty.
    pub async fn edit_repo(
        &self,
        owner: &str,
        name: &str,
        edit: &RepoEdit,
    ) -> Result<(), ForgeError> {
        if edit.is_empty() {
            return Ok(());
        }
        let raw = self
            .send_json(
                Method::PATCH,
                ROUTE_REPO,
                &["api", "v1", "repos", owner, name],
                Some(edit),
            )
            .await?;
        ensure_success(raw, ROUTE_REPO).map(|_| ())
    }

    /// True when the repository carries the [`MARKER_TOPIC`] topic.
    pub async fn has_marker(&self, owner: &str, name: &str) -> Result<bool, ForgeError> {
        let raw = self
            .get(
                ROUTE_TOPICS,
                &["api", "v1", "repos", owner, name, "topics"],
                &[("limit", TOPICS_LIMIT)],
            )
            .await?;
        let raw = ensure_success(raw, ROUTE_TOPICS)?;
        let topics: WireTopics = decode(&raw, ROUTE_TOPICS)?;
        Ok(topics.topics.iter().any(|t| t == MARKER_TOPIC))
    }

    /// `PUT /api/v1/repos/{owner}/{repo}/topics/ferry-mirror`. Idempotent.
    pub async fn add_marker(&self, owner: &str, name: &str) -> Result<(), ForgeError> {
        let raw = self
            .send_json(
                Method::PUT,
                ROUTE_TOPIC,
                &["api", "v1", "repos", owner, name, "topics", MARKER_TOPIC],
                None::<&()>,
            )
            .await?;
        ensure_success(raw, ROUTE_TOPIC).map(|_| ())
    }
}
