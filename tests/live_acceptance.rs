//! Live acceptance against real GitHub and Forgejo. Ignored by default.
//!
//! ```sh
//! cargo test --test live_acceptance -- --ignored --nocapture
//! ```
//!
//! This is the first test with no mock on either side. It proves the HTTP
//! credential path (askpass), the LFS batch API, and that Forgejo accepts an
//! LFS push to a repository that has no refs yet.
//!
//! Needs a LAN connection to Forgejo, `git-lfs`, and:
//!
//! | Variable | Meaning |
//! |---|---|
//! | `FERRY_LIVE_GITHUB_REPO` | `owner/name` of a small GitHub repository with at least one LFS-tracked file |
//! | `FERRY_LIVE_FORGEJO_OWNER` | Forgejo user or organization that receives the mirror |
//! | `FERRY_FORGEJO_TOKEN_FILE` | Forgejo token file |
//! | `FERRY_GITHUB_TOKEN_FILE` | GitHub token file (optional for a public repository) |
//! | `FERRY_LIVE_FORGEJO_URL` | required |
//! | `FERRY_LIVE_FORGEJO_USER` | optional, default `ferry`: the username paired with the token |
//! | `FERRY_LIVE_FORGEJO_NAME` | optional: reuse this destination instead of a new `ferry-acceptance-<random>` |
//!
//! Manual extension: force-push a branch, delete a tag, and rename the
//! default branch on GitHub, then run the test again with
//! `FERRY_LIVE_FORGEJO_NAME` set to the name the first run printed.
//!
//! The test never deletes the Forgejo repository. It prints the name; the
//! owner deletes it in the Forgejo UI.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use ferry::config::{
    Config, ForgejoConfig, GithubConfig, HealthConfig, RepoEntry, SyncConfig, TokenFiles,
};
use ferry::forge::{ForgejoClient, GithubClient, http_client};
use ferry::git::{Git, GitRunner, Remote, Side};
use ferry::sync::{SyncContext, SyncOutcome, sync_repo};
use tokio_util::sync::CancellationToken;

fn required(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("{name} must be set for the live acceptance test"))
}

fn optional(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Clones `url` with LFS smudging on and runs `git lfs fsck` in the clone.
/// Credentials come from askpass mode, exactly as they do for ferry itself.
fn clone_and_fsck(url: &str, into: &Path, runner: &GitRunner) {
    let settings = runner.settings();
    let git = |dir: &Path, args: &[&str]| {
        let mut command = Command::new("git");
        command
            .args([
                "-c",
                "credential.helper=",
                "-c",
                "filter.lfs.clean=git-lfs clean -- %f",
                "-c",
                "filter.lfs.smudge=git-lfs smudge -- %f",
                "-c",
                "filter.lfs.process=git-lfs filter-process",
                "-c",
                "filter.lfs.required=true",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_LFS_SKIP_SMUDGE", "0")
            .env("GIT_ASKPASS", env!("CARGO_BIN_EXE_ferry"))
            .env("FERRY_ASKPASS", "1")
            .env("FERRY_ASKPASS_GITHUB_HOST", &settings.github.host)
            .env("FERRY_ASKPASS_GITHUB_SCHEME", &settings.github.scheme)
            .env("FERRY_ASKPASS_FORGEJO_HOST", &settings.forgejo.host)
            .env("FERRY_ASKPASS_FORGEJO_SCHEME", &settings.forgejo.scheme)
            .env("FERRY_ASKPASS_FORGEJO_USER", &settings.forgejo_user);
        let output = command.output().expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let parent = into.parent().expect("clone parent");
    git(parent, &["clone", "--quiet", url, &into.to_string_lossy()]);
    git(into, &["lfs", "fsck"]);
}

#[tokio::test]
#[ignore = "needs real GitHub and Forgejo credentials; see the file header"]
async fn mirrors_a_real_repository_with_lfs() {
    let github_repo = required("FERRY_LIVE_GITHUB_REPO");
    let forgejo_owner = required("FERRY_LIVE_FORGEJO_OWNER");
    let forgejo_name = optional(
        "FERRY_LIVE_FORGEJO_NAME",
        &format!("ferry-acceptance-{:08x}", fastrand::u32(..)),
    );
    let scratch = tempfile::tempdir().expect("tempdir");

    let config = Config {
        sync: SyncConfig {
            cache_dir: scratch.path().join("cache"),
            ..SyncConfig::default()
        },
        github: GithubConfig::default(),
        forgejo: ForgejoConfig {
            url: required("FERRY_LIVE_FORGEJO_URL"),
            username: optional("FERRY_LIVE_FORGEJO_USER", "ferry"),
        },
        health: HealthConfig::default(),
        repos: vec![RepoEntry {
            github: github_repo.clone(),
            forgejo: format!("{forgejo_owner}/{forgejo_name}"),
            lfs: true,
            actions: false,
            adopt: false,
        }],
    };
    config.validate().expect("the live config is valid");
    let entry = &config.repos[0];
    println!(
        "Forgejo destination: {} (delete it by hand afterwards)",
        entry.forgejo
    );

    let token_files = TokenFiles::from_env();
    let tokens = token_files
        .load()
        .expect("the Forgejo token file is readable");
    let runner = GitRunner::new(
        ferry::git::GitSettings::from_config(
            &config,
            &token_files,
            &tokens,
            CancellationToken::new(),
        )
        .expect("git runner"),
    )
    .with_askpass_path(env!("CARGO_BIN_EXE_ferry"));
    runner
        .lfs_version()
        .await
        .expect("git-lfs must be installed for the live acceptance test");

    let http = http_client().expect("http client");
    let forgejo = ForgejoClient::new(http.clone(), &config.forgejo.url, tokens.forgejo.clone());
    let ctx = SyncContext::new(
        &config,
        Arc::new(runner.clone()) as Arc<dyn Git>,
        GithubClient::new(http.clone(), &config.github.api_url, tokens.github.clone()),
        ForgejoClient::new(http, &config.forgejo.url, tokens.forgejo.clone()),
    );

    let first: SyncOutcome = sync_repo(&ctx, entry).await;
    println!("first pass: {first:?}");
    assert!(matches!(first.result_tag(), "synced" | "noop"), "{first:?}");

    let source = Remote {
        url: format!("{}/{github_repo}.git", config.github.git_url),
        side: Side::Github,
    };
    let dest = Remote {
        url: format!(
            "{}/{}.git",
            config.forgejo.url.trim_end_matches('/'),
            entry.forgejo
        ),
        side: Side::Forgejo,
    };
    let src = runner.ls_remote(&source).await.expect("ls-remote GitHub");
    let dst = runner.ls_remote(&dest).await.expect("ls-remote Forgejo");
    assert!(src.refs.has_heads(), "the test repository has no branches");
    assert_eq!(dst.refs, src.refs, "ref maps differ after the sync");

    let (owner, name) = entry.forgejo_parts();
    let repo = forgejo
        .get_repo(owner, name)
        .await
        .expect("read the Forgejo repository")
        .expect("the Forgejo repository exists");
    assert!(repo.private, "a repository ferry creates must be private");
    assert!(!repo.mirror);
    assert!(
        forgejo.has_marker(owner, name).await.expect("read topics"),
        "the ferry-mirror topic is missing"
    );
    assert_eq!(Some(repo.default_branch), src.head, "default branch");

    // Assumption A2 and the LFS credential path: every LFS object arrived.
    clone_and_fsck(&dest.url, &scratch.path().join("clone"), &runner);

    let second = sync_repo(&ctx, entry).await;
    println!("second pass: {second:?}");
    assert_eq!(second.result_tag(), "noop", "{second:?}");
}
