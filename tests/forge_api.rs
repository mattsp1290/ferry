//! HTTP tests for the GitHub and Forgejo clients, against `wiremock`.

use std::time::Duration;

use ferry::config::Token;
use ferry::forge::{
    CreateOutcome, ForgeError, ForgejoClient, GithubClient, MARKER_TOPIC, RepoEdit, http_client,
};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const FORGEJO_TOKEN: &str = "test-forgejo-token-not-real";
const GITHUB_TOKEN: &str = "test-github-token-not-real";
const UA: &str = concat!("ferry/", env!("CARGO_PKG_VERSION"));

fn forgejo(server: &MockServer) -> ForgejoClient {
    ForgejoClient::new(
        http_client().unwrap(),
        &format!("{}/", server.uri()),
        Token::new(FORGEJO_TOKEN),
    )
}

fn github(server: &MockServer, token: Option<&str>) -> GithubClient {
    GithubClient::new(http_client().unwrap(), &server.uri(), token.map(Token::new))
}

fn repo_json() -> Value {
    json!({
        "default_branch": "main",
        "description": "d",
        "private": true,
        "mirror": false,
        "empty": false,
        "has_actions": true,
        "topics": ["x"],
    })
}

async fn mock_whoami(server: &MockServer, login: &str) {
    Mock::given(method("GET"))
        .and(path("/api/v1/user"))
        .and(header("authorization", format!("token {FORGEJO_TOKEN}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": login })))
        .mount(server)
        .await;
}

async fn requests(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap()
}

// ---------------------------------------------------------------- GitHub

#[tokio::test]
async fn github_get_repo_happy_path_with_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octo/hello"))
        .and(header("authorization", format!("Bearer {GITHUB_TOKEN}")))
        .and(header("accept", "application/vnd.github+json"))
        .and(header("user-agent", UA))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "description": "hello world", "private": true, "archived": true, "extra": 1
        })))
        .expect(1)
        .mount(&server)
        .await;
    let meta = github(&server, Some(GITHUB_TOKEN))
        .get_repo("octo", "hello")
        .await
        .unwrap();
    assert_eq!(meta.description, "hello world");
    assert!(meta.private && meta.archived);
}

#[tokio::test]
async fn github_null_description_becomes_empty_and_no_token_sends_no_auth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octo/hello"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "description": null, "private": false, "archived": false })),
        )
        .mount(&server)
        .await;
    let meta = github(&server, None)
        .get_repo("octo", "hello")
        .await
        .unwrap();
    assert_eq!(meta.description, "");
    let reqs = requests(&server).await;
    assert_eq!(reqs.len(), 1);
    assert!(!reqs[0].headers.contains_key("authorization"));
    assert_eq!(
        reqs[0].headers.get("accept").unwrap(),
        "application/vnd.github+json"
    );
}

async fn github_status(
    status: u16,
    headers: &[(&str, &str)],
    body: &str,
) -> Result<ferry::forge::SourceMeta, ForgeError> {
    let server = MockServer::start().await;
    let mut resp = ResponseTemplate::new(status).set_body_string(body);
    for (k, v) in headers {
        resp = resp.insert_header(*k, *v);
    }
    Mock::given(method("GET"))
        .respond_with(resp)
        .mount(&server)
        .await;
    github(&server, Some(GITHUB_TOKEN)).get_repo("o", "r").await
}

#[tokio::test]
async fn github_error_mapping() {
    assert!(matches!(
        github_status(404, &[], "").await,
        Err(ForgeError::NotFound { .. })
    ));
    assert!(matches!(
        github_status(401, &[], "bad credentials").await,
        Err(ForgeError::Auth { status: 401, .. })
    ));
    // 403 without rate-limit headers is an authorization failure.
    assert!(matches!(
        github_status(403, &[("x-ratelimit-remaining", "12")], "").await,
        Err(ForgeError::Auth { status: 403, .. })
    ));
    assert!(matches!(
        github_status(500, &[], "boom").await,
        Err(ForgeError::Server { status: 500, .. })
    ));
    assert!(matches!(
        github_status(503, &[], "").await,
        Err(ForgeError::Server { status: 503, .. })
    ));
    assert!(matches!(
        github_status(418, &[], "").await,
        Err(ForgeError::Unexpected {
            status: Some(418),
            ..
        })
    ));
    assert!(matches!(
        github_status(200, &[], "not json").await,
        Err(ForgeError::Unexpected { .. })
    ));
}

#[tokio::test]
async fn github_rate_limit_variants() {
    // 403 + remaining 0 + retry-after seconds.
    assert!(matches!(
        github_status(403, &[("x-ratelimit-remaining", "0"), ("retry-after", "42")], "").await,
        Err(ForgeError::RateLimited { retry_after: Some(d), .. }) if d == Duration::from_secs(42)
    ));
    // 403 + retry-after alone.
    assert!(matches!(
        github_status(403, &[("retry-after", "7")], "").await,
        Err(ForgeError::RateLimited { retry_after: Some(d), .. }) if d == Duration::from_secs(7)
    ));
    // 429 + reset epoch in the future.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let reset = (now + 120).to_string();
    match github_status(
        429,
        &[
            ("x-ratelimit-remaining", "0"),
            ("x-ratelimit-reset", &reset),
        ],
        "",
    )
    .await
    {
        Err(ForgeError::RateLimited {
            retry_after: Some(d),
            ..
        }) => {
            assert!(
                d > Duration::from_secs(100) && d <= Duration::from_secs(120),
                "{d:?}"
            );
        }
        other => panic!("unexpected {other:?}"),
    }
    // Remaining 0 with a reset in the past clamps to zero.
    assert!(matches!(
        github_status(403, &[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "1")], "").await,
        Err(ForgeError::RateLimited { retry_after: Some(d), .. }) if d == Duration::ZERO
    ));
    // Remaining 0 with nothing else: unknown wait.
    assert!(matches!(
        github_status(403, &[("x-ratelimit-remaining", "0")], "").await,
        Err(ForgeError::RateLimited {
            retry_after: None,
            ..
        })
    ));
    // Bare 429.
    assert!(matches!(
        github_status(429, &[], "").await,
        Err(ForgeError::RateLimited {
            retry_after: None,
            ..
        })
    ));
}

#[tokio::test]
async fn github_network_error() {
    let url = closed_port_url().await;
    let err = GithubClient::new(http_client().unwrap(), &url, None)
        .get_repo("o", "r")
        .await
        .unwrap_err();
    assert!(matches!(err, ForgeError::Network { .. }), "{err:?}");
}

async fn closed_port_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

// --------------------------------------------------------------- Forgejo

#[tokio::test]
async fn forgejo_get_repo_happy_path_and_headers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/me/proj"))
        .and(header("authorization", format!("token {FORGEJO_TOKEN}")))
        .and(header("user-agent", UA))
        .respond_with(ResponseTemplate::new(200).set_body_json(repo_json()))
        .expect(1)
        .mount(&server)
        .await;
    let repo = forgejo(&server)
        .get_repo("me", "proj")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repo.default_branch, "main");
    assert_eq!(repo.description, "d");
    assert!(repo.private && !repo.mirror && !repo.empty && repo.has_actions);
    assert_eq!(repo.topics, vec!["x"]);
}

#[tokio::test]
async fn forgejo_get_repo_tolerates_missing_and_null_fields() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/me/proj"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "default_branch": "trunk", "description": null, "private": true,
            "mirror": false, "empty": true, "topics": null
        })))
        .mount(&server)
        .await;
    let repo = forgejo(&server)
        .get_repo("me", "proj")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repo.description, "");
    assert!(!repo.has_actions);
    assert!(repo.topics.is_empty());
    assert!(repo.empty);
}

#[tokio::test]
async fn forgejo_get_repo_404_is_none() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    assert_eq!(forgejo(&server).get_repo("me", "gone").await.unwrap(), None);
}

#[tokio::test]
async fn forgejo_paths_are_percent_encoded() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    forgejo(&server).get_repo("a b", "c/d").await.unwrap();
    let reqs = requests(&server).await;
    assert_eq!(reqs[0].url.path(), "/api/v1/repos/a%20b/c%2Fd");
}

async fn forgejo_status(
    status: u16,
    headers: &[(&str, &str)],
    body: &str,
) -> Result<String, ForgeError> {
    let server = MockServer::start().await;
    let mut resp = ResponseTemplate::new(status).set_body_string(body);
    for (k, v) in headers {
        resp = resp.insert_header(*k, *v);
    }
    Mock::given(method("GET"))
        .respond_with(resp)
        .mount(&server)
        .await;
    forgejo(&server).whoami().await
}

#[tokio::test]
async fn forgejo_error_mapping() {
    assert!(matches!(
        forgejo_status(404, &[], "").await,
        Err(ForgeError::NotFound { .. })
    ));
    assert!(matches!(
        forgejo_status(401, &[], "nope").await,
        Err(ForgeError::Auth { status: 401, .. })
    ));
    assert!(matches!(
        forgejo_status(403, &[], "nope").await,
        Err(ForgeError::Auth { status: 403, .. })
    ));
    assert!(matches!(
        forgejo_status(429, &[("retry-after", "9")], "").await,
        Err(ForgeError::RateLimited { retry_after: Some(d), .. }) if d == Duration::from_secs(9)
    ));
    assert!(matches!(
        forgejo_status(429, &[], "").await,
        Err(ForgeError::RateLimited {
            retry_after: None,
            ..
        })
    ));
    assert!(matches!(
        forgejo_status(502, &[], "bad gateway").await,
        Err(ForgeError::Server { status: 502, .. })
    ));
    assert!(matches!(
        forgejo_status(302, &[], "").await,
        Err(ForgeError::Unexpected {
            status: Some(302),
            ..
        })
    ));
    assert!(matches!(
        forgejo_status(200, &[], "<html>").await,
        Err(ForgeError::Unexpected { .. })
    ));
    assert!(matches!(
        forgejo_status(200, &[], "{}").await,
        Err(ForgeError::Unexpected { .. })
    ));
}

#[tokio::test]
async fn forgejo_error_body_excerpt_is_capped() {
    let big = "x".repeat(5000);
    let err = forgejo_status(500, &[], &big).await.unwrap_err();
    assert!(err.to_string().len() < 700, "{}", err.to_string().len());
}

#[tokio::test]
async fn forgejo_network_error() {
    let url = closed_port_url().await;
    let client = ForgejoClient::new(http_client().unwrap(), &url, Token::new(FORGEJO_TOKEN));
    let err = client.get_repo("o", "r").await.unwrap_err();
    assert!(matches!(err, ForgeError::Network { .. }), "{err:?}");
    let text = format!("{err} {err:?}");
    assert!(!text.contains(FORGEJO_TOKEN));
    assert!(!text.contains("http://"), "{text}");
}

#[tokio::test]
async fn whoami_is_cached_after_success() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "me" })))
        .expect(1)
        .mount(&server)
        .await;
    let client = forgejo(&server);
    assert_eq!(client.whoami().await.unwrap(), "me");
    assert_eq!(client.whoami().await.unwrap(), "me");
}

#[tokio::test]
async fn whoami_failure_is_not_cached() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/user"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "me" })))
        .mount(&server)
        .await;
    let client = forgejo(&server);
    assert!(client.whoami().await.is_err());
    assert_eq!(client.whoami().await.unwrap(), "me");
}

#[tokio::test]
async fn create_repo_for_user_uses_user_endpoint_case_insensitively() {
    let server = MockServer::start().await;
    mock_whoami(&server, "Me").await;
    Mock::given(method("POST"))
        .and(path("/api/v1/user/repos"))
        .and(header("authorization", format!("token {FORGEJO_TOKEN}")))
        .and(body_json(json!({
            "name": "proj", "private": true, "auto_init": false, "description": "desc"
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(repo_json()))
        .expect(1)
        .mount(&server)
        .await;
    let (repo, outcome) = forgejo(&server)
        .create_repo("me", "proj", "desc")
        .await
        .unwrap();
    assert_eq!(outcome, CreateOutcome::Created);
    assert_eq!(repo.default_branch, "main");
}

#[tokio::test]
async fn create_repo_for_organization_uses_org_endpoint() {
    let server = MockServer::start().await;
    mock_whoami(&server, "me").await;
    Mock::given(method("POST"))
        .and(path("/api/v1/orgs/acme/repos"))
        .and(body_json(json!({
            "name": "proj", "private": true, "auto_init": false, "description": ""
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(repo_json()))
        .expect(1)
        .mount(&server)
        .await;
    let (_, outcome) = forgejo(&server)
        .create_repo("acme", "proj", "")
        .await
        .unwrap();
    assert_eq!(outcome, CreateOutcome::Created);
}

#[tokio::test]
async fn create_repo_conflict_rereads_existing() {
    let server = MockServer::start().await;
    mock_whoami(&server, "me").await;
    Mock::given(method("POST"))
        .and(path("/api/v1/user/repos"))
        .respond_with(ResponseTemplate::new(409).set_body_string("exists"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/me/proj"))
        .respond_with(ResponseTemplate::new(200).set_body_json(repo_json()))
        .expect(1)
        .mount(&server)
        .await;
    let (repo, outcome) = forgejo(&server)
        .create_repo("me", "proj", "")
        .await
        .unwrap();
    assert_eq!(outcome, CreateOutcome::AlreadyExisted);
    assert_eq!(repo.default_branch, "main");
}

#[tokio::test]
async fn create_repo_conflict_then_missing_is_unexpected() {
    let server = MockServer::start().await;
    mock_whoami(&server, "me").await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(409))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/me/proj"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let err = forgejo(&server)
        .create_repo("me", "proj", "")
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            ForgeError::Unexpected {
                status: Some(409),
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn create_repo_error_mapping() {
    let server = MockServer::start().await;
    mock_whoami(&server, "me").await;
    Mock::given(method("POST"))
        .and(path("/api/v1/orgs/acme/repos"))
        .respond_with(ResponseTemplate::new(403).set_body_string("not an org member"))
        .mount(&server)
        .await;
    let err = forgejo(&server)
        .create_repo("acme", "proj", "")
        .await
        .unwrap_err();
    assert!(
        matches!(err, ForgeError::Auth { status: 403, .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn edit_repo_sends_only_changed_fields() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/repos/me/proj"))
        .and(header("authorization", format!("token {FORGEJO_TOKEN}")))
        .and(body_json(
            json!({ "description": "new", "has_actions": false }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(repo_json()))
        .expect(1)
        .mount(&server)
        .await;
    forgejo(&server)
        .edit_repo(
            "me",
            "proj",
            &RepoEdit {
                description: Some("new".into()),
                has_actions: Some(false),
                ..RepoEdit::default()
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn edit_repo_default_branch_only() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(body_json(json!({ "default_branch": "trunk" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(repo_json()))
        .expect(1)
        .mount(&server)
        .await;
    forgejo(&server)
        .edit_repo(
            "me",
            "proj",
            &RepoEdit {
                default_branch: Some("trunk".into()),
                ..RepoEdit::default()
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn edit_repo_with_no_changes_sends_no_request() {
    let server = MockServer::start().await;
    forgejo(&server)
        .edit_repo("me", "proj", &RepoEdit::default())
        .await
        .unwrap();
    assert!(requests(&server).await.is_empty());
}

#[tokio::test]
async fn edit_repo_error_maps() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let err = forgejo(&server)
        .edit_repo(
            "me",
            "proj",
            &RepoEdit {
                has_actions: Some(true),
                ..RepoEdit::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ForgeError::NotFound { .. }));
}

#[tokio::test]
async fn has_marker_present_and_absent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/me/marked/topics"))
        .and(query_param("limit", "100"))
        .and(header("authorization", format!("token {FORGEJO_TOKEN}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "topics": ["a", MARKER_TOPIC] })),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/me/plain/topics"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "topics": ["a"] })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/me/none/topics"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "topics": null })))
        .mount(&server)
        .await;
    let client = forgejo(&server);
    assert!(client.has_marker("me", "marked").await.unwrap());
    assert!(!client.has_marker("me", "plain").await.unwrap());
    assert!(!client.has_marker("me", "none").await.unwrap());
}

#[tokio::test]
async fn has_marker_missing_repo_is_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let err = forgejo(&server).has_marker("me", "x").await.unwrap_err();
    assert!(matches!(err, ForgeError::NotFound { .. }));
}

#[tokio::test]
async fn add_marker_puts_topic() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/repos/me/proj/topics/ferry-mirror"))
        .and(header("authorization", format!("token {FORGEJO_TOKEN}")))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    forgejo(&server).add_marker("me", "proj").await.unwrap();
    assert_eq!(MARKER_TOPIC, "ferry-mirror");
}

#[tokio::test]
async fn add_marker_error_maps() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let err = forgejo(&server).add_marker("me", "proj").await.unwrap_err();
    assert!(matches!(err, ForgeError::Server { status: 500, .. }));
}

#[tokio::test]
async fn no_delete_request_is_ever_sent() {
    let server = MockServer::start().await;
    mock_whoami(&server, "me").await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200).set_body_json(repo_json()))
        .mount(&server)
        .await;
    let client = forgejo(&server);
    let _ = client.get_repo("me", "p").await;
    let _ = client.create_repo("me", "p", "").await;
    let _ = client.has_marker("me", "p").await;
    let _ = client.add_marker("me", "p").await;
    for req in requests(&server).await {
        assert_ne!(req.method.as_str(), "DELETE");
    }
}

// ------------------------------------------------------- secret hygiene

#[tokio::test]
async fn token_appears_in_no_debug_or_display_output() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(403).set_body_string("denied"))
        .mount(&server)
        .await;

    let fj = forgejo(&server);
    let gh = github(&server, Some(GITHUB_TOKEN));
    let mut text = format!("{fj:?} {gh:?}");

    let closed = closed_port_url().await;
    let dead_fj = ForgejoClient::new(http_client().unwrap(), &closed, Token::new(FORGEJO_TOKEN));
    let dead_gh = GithubClient::new(
        http_client().unwrap(),
        &closed,
        Some(Token::new(GITHUB_TOKEN)),
    );
    text.push_str(&format!("{dead_fj:?} {dead_gh:?}"));

    let errors = [
        fj.whoami().await.unwrap_err(),
        fj.get_repo("o", "r").await.unwrap_err(),
        fj.create_repo("o", "r", "").await.unwrap_err(),
        fj.has_marker("o", "r").await.unwrap_err(),
        gh.get_repo("o", "r").await.unwrap_err(),
        dead_fj.get_repo("o", "r").await.unwrap_err(),
        dead_gh.get_repo("o", "r").await.unwrap_err(),
    ];
    for err in &errors {
        text.push_str(&format!("{err} {err:?}"));
    }
    assert!(!text.contains(FORGEJO_TOKEN), "{text}");
    assert!(!text.contains(GITHUB_TOKEN), "{text}");
    assert!(text.contains("[redacted]"));
}
