//! Parsing and validation of `ferry.toml`, and token-file loading.

use std::path::{Path, PathBuf};
use std::process::Command;

use ferry::config::{
    Config, ConfigError, RepoEntry, TokenFiles, load_forgejo_token, load_github_token,
};

const MINIMAL: &str = r#"
[forgejo]
url = "https://git.example.com"
username = "ferry"
"#;

fn parse(text: &str) -> Config {
    toml::from_str(text).expect("config parses")
}

fn violations(text: &str) -> Vec<String> {
    match parse(text).validate_with(false) {
        Err(ConfigError::Invalid(violations)) => violations,
        other => panic!("expected violations, got {other:?}"),
    }
}

/// `MINIMAL` plus one `[[repos]]` entry.
fn with_repo(github: &str, forgejo: &str) -> String {
    format!("{MINIMAL}\n[[repos]]\ngithub = \"{github}\"\nforgejo = \"{forgejo}\"\n")
}

fn example_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/ferry.toml")
}

#[test]
fn example_file_parses_and_validates() {
    let config = Config::load(&example_path()).expect("example loads");
    config.validate_with(false).expect("example is valid");
    assert_eq!(config.repos.len(), 2);
    assert_eq!(
        config.repos[0],
        RepoEntry {
            github: "example-owner/example-repo".to_string(),
            forgejo: "example-owner/example-repo".to_string(),
            lfs: true,
            actions: false,
            adopt: false,
        }
    );
    assert!(!config.repos[1].lfs);
}

#[test]
fn example_file_spells_out_the_defaults() {
    let example = Config::load(&example_path()).expect("example loads");
    let minimal = parse(MINIMAL);
    assert_eq!(example.sync, minimal.sync);
    assert_eq!(example.github, minimal.github);
    assert_eq!(example.health, minimal.health);
}

#[test]
fn defaults_apply() {
    let config = parse(&with_repo("a/b", "c/d"));
    config.validate_with(false).expect("valid");
    assert_eq!(config.sync.poll_interval_seconds, 300);
    assert_eq!(config.sync.metadata_interval_seconds, 3600);
    assert_eq!(config.sync.max_concurrency, 2);
    assert_eq!(config.sync.git_timeout_seconds, 1800);
    assert_eq!(config.sync.cache_dir, Path::new("/var/lib/ferry/cache"));
    assert_eq!(config.github.api_url, "https://api.github.com");
    assert_eq!(config.github.git_url, "https://github.com");
    assert_eq!(config.health.listen, "0.0.0.0:8080");
    let entry = &config.repos[0];
    assert!(entry.lfs);
    assert!(!entry.actions);
    assert!(!entry.adopt);
}

#[test]
fn empty_allowlist_is_valid() {
    let config = parse(MINIMAL);
    config.validate_with(false).expect("valid");
    assert!(config.repos.is_empty());
}

#[test]
fn unknown_keys_are_rejected() {
    for text in [
        format!("{MINIMAL}\nunknown = 1\n"),
        format!("{MINIMAL}\n[sync]\nunknown = 1\n"),
        "[forgejo]\nurl = \"https://x\"\nusername = \"u\"\ntoken = \"t\"\n".to_string(),
        format!("{MINIMAL}\n[[repos]]\ngithub = \"a/b\"\nforgejo = \"c/d\"\nwiki = true\n"),
    ] {
        assert!(
            toml::from_str::<Config>(&text).is_err(),
            "accepted an unknown key in:\n{text}"
        );
    }
}

#[test]
fn missing_forgejo_section_is_a_parse_error() {
    assert!(toml::from_str::<Config>("").is_err());
}

#[test]
fn rule_1_repo_name_form() {
    let cases = [
        ("noslash", "must have the form owner/name"),
        ("a/b/c", "only letters"),
        ("a/", "only letters"),
        ("/b", "only letters"),
        ("a b/c", "only letters"),
        ("a/b?x", "only letters"),
        ("../b", "'.' or '..'"),
        ("a/.", "'.' or '..'"),
        ("a/b.git", "must not end in .git"),
        ("a/b.GIT", "must not end in .git"),
    ];
    for (name, fragment) in cases {
        let found = violations(&with_repo(name, "ok/ok"));
        assert_eq!(found.len(), 1, "{name}: {found:?}");
        assert!(
            found[0].contains("repos[0].github") && found[0].contains(fragment),
            "{name}: {found:?}"
        );

        let found = violations(&with_repo("ok/ok", name));
        assert_eq!(found.len(), 1, "{name}: {found:?}");
        assert!(found[0].contains("repos[0].forgejo"), "{name}: {found:?}");
    }
}

#[test]
fn rule_2_duplicates_are_case_insensitive() {
    let text = format!(
        "{MINIMAL}
[[repos]]
github = \"Owner/Repo\"
forgejo = \"dest/one\"
[[repos]]
github = \"owner/repo\"
forgejo = \"dest/two\"
[[repos]]
github = \"owner/other\"
forgejo = \"DEST/ONE\"
"
    );
    let found = violations(&text);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found[0].contains("repos[1].github") && found[0].contains("duplicates repos[0]"));
    assert!(found[1].contains("repos[2].forgejo") && found[1].contains("duplicates repos[0]"));
}

#[test]
fn rule_3_urls_must_be_https_without_userinfo() {
    let config = |forgejo: &str, api: &str, git: &str| {
        format!(
            "[forgejo]\nurl = \"{forgejo}\"\nusername = \"ferry\"\n[github]\napi_url = \"{api}\"\ngit_url = \"{git}\"\n"
        )
    };
    let ok = "https://example.com";

    let found = violations(&config("http://git.example.com", ok, ok));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("forgejo.url must use https"));

    let found = violations(&config(ok, "ssh://api.example.com", ok));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("github.api_url must use https, not ssh"));

    let found = violations(&config(ok, ok, "https://user:hunter2@github.com"));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("github.git_url must not contain userinfo"));
    assert!(
        !found[0].contains("hunter2"),
        "userinfo leaked into the message"
    );

    let found = violations(&config("not a url", ok, ok));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("forgejo.url"));
}

#[test]
fn rule_3_http_is_accepted_only_with_the_insecure_switch() {
    let text = "[forgejo]\nurl = \"http://127.0.0.1:3000\"\nusername = \"ferry\"\n";
    assert!(parse(text).validate_with(false).is_err());
    parse(text)
        .validate_with(true)
        .expect("http passes with the switch");
}

#[test]
fn rule_4_numeric_ranges() {
    let cases = [
        ("poll_interval_seconds = 29", "sync.poll_interval_seconds"),
        (
            "metadata_interval_seconds = 299",
            "sync.metadata_interval_seconds",
        ),
        ("max_concurrency = 0", "sync.max_concurrency"),
        ("max_concurrency = 9", "sync.max_concurrency"),
        ("git_timeout_seconds = 0", "sync.git_timeout_seconds"),
    ];
    for (line, field) in cases {
        let found = violations(&format!("{MINIMAL}\n[sync]\n{line}\n"));
        assert_eq!(found.len(), 1, "{line}: {found:?}");
        assert!(found[0].contains(field), "{line}: {found:?}");
    }
    for line in [
        "poll_interval_seconds = 30",
        "metadata_interval_seconds = 300",
        "max_concurrency = 1",
        "max_concurrency = 8",
    ] {
        parse(&format!("{MINIMAL}\n[sync]\n{line}\n"))
            .validate_with(false)
            .unwrap_or_else(|error| panic!("{line}: {error}"));
    }
}

#[test]
fn rule_5_cache_dir_must_be_absolute() {
    let found = violations(&format!("{MINIMAL}\n[sync]\ncache_dir = \"cache\"\n"));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("sync.cache_dir"));
}

#[test]
fn health_listen_must_be_a_socket_address() {
    let found = violations(&format!("{MINIMAL}\n[health]\nlisten = \"8080\"\n"));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("health.listen"));
}

#[test]
fn all_violations_are_reported() {
    let text = format!(
        "{MINIMAL}\n[sync]\npoll_interval_seconds = 1\nmax_concurrency = 99\n[[repos]]\ngithub = \"bad\"\nforgejo = \"a/b.git\"\n"
    );
    assert_eq!(violations(&text).len(), 4);
}

#[test]
fn select_repos_filters_by_github_name() {
    let text = format!(
        "{MINIMAL}\n[[repos]]\ngithub = \"Owner/One\"\nforgejo = \"d/one\"\n[[repos]]\ngithub = \"owner/two\"\nforgejo = \"d/two\"\n"
    );
    let config = parse(&text);
    assert_eq!(config.select_repos(&[]).expect("all").len(), 2);
    let selected = config
        .select_repos(&["owner/one".to_string()])
        .expect("selected");
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].github, "Owner/One");
    assert!(config.select_repos(&["owner/three".to_string()]).is_err());
}

// --- token files -----------------------------------------------------------

#[test]
fn github_token_unset_means_no_token() {
    assert_eq!(load_github_token(None), None);
}

#[test]
fn github_token_missing_file_means_no_token() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(load_github_token(Some(&dir.path().join("absent"))), None);
}

#[test]
fn github_token_empty_file_means_no_token() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("token");
    std::fs::write(&path, "\n").expect("write");
    assert_eq!(load_github_token(Some(&path)), None);
}

#[test]
fn token_files_trim_trailing_whitespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("token");
    std::fs::write(&path, "abc123 \r\n\n").expect("write");
    let token = load_github_token(Some(&path)).expect("token");
    assert_eq!(token.expose(), "abc123");
}

#[test]
fn forgejo_token_is_required() {
    let dir = tempfile::tempdir().expect("tempdir");
    let empty = dir.path().join("empty");
    std::fs::write(&empty, "").expect("write");

    assert!(load_forgejo_token(None).is_err());
    assert!(load_forgejo_token(Some(&dir.path().join("absent"))).is_err());
    assert!(load_forgejo_token(Some(&empty)).is_err());

    let path = dir.path().join("token");
    std::fs::write(&path, "forgejo-test-token\n").expect("write");
    let files = TokenFiles {
        github: None,
        forgejo: Some(path),
    };
    let tokens = files.load().expect("tokens");
    assert_eq!(tokens.forgejo.expose(), "forgejo-test-token");
    assert_eq!(tokens.github, None);
    assert_eq!(tokens.secrets().len(), 1);
}

// --- check-config through the binary -----------------------------------------

fn ferry() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferry"));
    command
        .env_remove("FERRY_CONFIG")
        .env_remove("FERRY_ASKPASS")
        .env_remove("FERRY_ALLOW_INSECURE_URLS");
    command
}

#[test]
fn check_config_accepts_the_example() {
    let output = ferry()
        .args(["check-config", "--config"])
        .arg(example_path())
        .output()
        .expect("runs");
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("2 repos"));
}

#[test]
fn check_config_reads_the_path_from_env() {
    let output = ferry()
        .arg("check-config")
        .env("FERRY_CONFIG", example_path())
        .output()
        .expect("runs");
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn check_config_prints_every_violation_and_exits_2() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ferry.toml");
    std::fs::write(
        &path,
        format!("{MINIMAL}\n[sync]\npoll_interval_seconds = 5\ncache_dir = \"relative\"\n"),
    )
    .expect("write");

    let output = ferry()
        .args(["check-config", "--config"])
        .arg(&path)
        .output()
        .expect("runs");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("sync.poll_interval_seconds"), "{stderr}");
    assert!(stderr.contains("sync.cache_dir"), "{stderr}");
}

#[test]
fn check_config_exits_2_for_an_unreadable_or_empty_file() {
    let output = ferry()
        .args(["check-config", "--config", "/nonexistent/ferry.toml"])
        .output()
        .expect("runs");
    assert_eq!(output.status.code(), Some(2), "{output:?}");

    let output = ferry()
        .args(["check-config", "--config", "/dev/null"])
        .output()
        .expect("runs");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}
