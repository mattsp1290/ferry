//! The deployment verifier's hidden refs command uses canonical remotes and
//! credentials while leaving the configured mirror cache untouched.

use std::path::PathBuf;
use std::process::Output;

use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod support;

use support::{FORGEJO_TOKEN, GITHUB_TOKEN, ferry_command};

const SOURCE_OID: &str = "1111111111111111111111111111111111111111";
const DEST_OID: &str = "2222222222222222222222222222222222222222";

struct RefsEnv {
    tmp: TempDir,
    config: PathBuf,
    cache: PathBuf,
}

impl RefsEnv {
    fn new(source: &str, dest: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache-is-a-file");
        std::fs::write(&cache, b"cache sentinel").unwrap();
        std::fs::write(tmp.path().join("github-token"), GITHUB_TOKEN).unwrap();
        std::fs::write(tmp.path().join("forgejo-token"), FORGEJO_TOKEN).unwrap();
        let config = tmp.path().join("ferry.toml");
        let toml_string = |text: &str| toml::Value::String(text.to_owned()).to_string();
        std::fs::write(
            &config,
            format!(
                "[github]\ngit_url = {}\n[forgejo]\nurl = {}\nusername = \"ferry\"\n[sync]\ncache_dir = {}\n[[repos]]\ngithub = \"source-owner/project\"\nforgejo = \"dest-owner/mirror\"\n",
                toml_string(source),
                toml_string(dest),
                toml_string(cache.to_str().unwrap()),
            ),
        )
        .unwrap();
        Self { tmp, config, cache }
    }

    fn refs(&self, side: &str, repo: &str) -> Output {
        ferry_command()
            .env("FERRY_ALLOW_INSECURE_URLS", "1")
            .env(
                "FERRY_GITHUB_TOKEN_FILE",
                self.tmp.path().join("github-token"),
            )
            .env(
                "FERRY_FORGEJO_TOKEN_FILE",
                self.tmp.path().join("forgejo-token"),
            )
            .args(["refs", "--config"])
            .arg(&self.config)
            .args(["--side", side, repo])
            .output()
            .unwrap()
    }

    fn assert_cache_untouched(&self) {
        assert!(self.cache.is_file());
        assert_eq!(std::fs::read(&self.cache).unwrap(), b"cache sentinel");
        let mut names: Vec<_> = std::fs::read_dir(self.tmp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "cache-is-a-file",
                "ferry.toml",
                "forgejo-token",
                "github-token"
            ]
        );
    }
}

async fn serve_dumb_refs(server: &MockServer, prefix: &str, oid: &str) {
    Mock::given(method("GET"))
        .and(path(format!("{prefix}/info/refs")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!("{oid}\trefs/heads/main\n{oid}\trefs/tags/v1\n")),
        )
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{prefix}/HEAD")))
        .respond_with(ResponseTemplate::new(200).set_body_string("ref: refs/heads/main\n"))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn refs_lists_each_canonical_configured_remote_without_touching_cache() {
    let github = MockServer::start().await;
    let forgejo = MockServer::start().await;
    let source_path = "/custom-source/source-owner/project.git";
    let dest_path = "/custom-dest/dest-owner/mirror.git";
    serve_dumb_refs(&github, source_path, SOURCE_OID).await;
    serve_dumb_refs(&forgejo, dest_path, DEST_OID).await;
    let env = RefsEnv::new(
        &format!("{}/custom-source/", github.uri()),
        &format!("{}/custom-dest/", forgejo.uri()),
    );
    for (side, oid) in [("github", SOURCE_OID), ("forgejo", DEST_OID)] {
        let output = env.refs(side, "SOURCE-OWNER/PROJECT");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("{oid}\trefs/heads/main\n{oid}\trefs/tags/v1\n")
        );
        assert!(output.stderr.is_empty(), "{output:?}");
        env.assert_cache_untouched();
    }
    for server in [&github, &forgejo] {
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|request| request.method.as_str() == "GET")
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.url.path().contains("/api/"))
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn refs_rejects_unknown_allowlist_entry_before_contacting_remotes() {
    let server = MockServer::start().await;
    let env = RefsEnv::new(&server.uri(), &server.uri());
    let output = env.refs("github", "unknown/repo");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown/repo"));
    assert!(server.received_requests().await.unwrap().is_empty());
    env.assert_cache_untouched();
}

#[tokio::test(flavor = "multi_thread")]
async fn refs_real_git_failure_exits_one_and_redacts_loaded_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    // Git includes this URL in stderr. The loaded fake token must be removed
    // even when it appears in a remote path rather than an auth header.
    let env = RefsEnv::new(&format!("{}/{GITHUB_TOKEN}", server.uri()), &server.uri());
    let output = env.refs("github", "source-owner/project");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("[redacted]"), "{stderr}");
    assert!(!stderr.contains(GITHUB_TOKEN), "{stderr}");
    assert!(!stderr.contains(FORGEJO_TOKEN), "{stderr}");
    assert!(!server.received_requests().await.unwrap().is_empty());
    env.assert_cache_untouched();
}
