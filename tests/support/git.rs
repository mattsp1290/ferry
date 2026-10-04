//! Real git over `file://`: hermetic runner and GitHub-side source repositories.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use ferry::config::{Token, TokenFiles};
use ferry::git::{GitSettings, RefMap};
use tokio_util::sync::CancellationToken;

use super::{FORGEJO_LOGIN, FORGEJO_TOKEN};

/// Runs plain `git` hermetically and returns trimmed stdout.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

pub fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// Writes an executable `#!/bin/sh` script running `body`.
pub fn write_script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("write script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod script");
}

/// Settings for a `GitRunner` against fake hosts. Callers override the hosts,
/// user, token files, and secrets they care about.
pub fn git_settings(
    cache_dir: PathBuf,
    timeout: Duration,
    cancel: CancellationToken,
) -> GitSettings {
    GitSettings {
        cache_dir,
        timeout,
        kill_grace: Duration::from_millis(500),
        token_files: TokenFiles::default(),
        github: ferry::git::askpass::Origin {
            host: "github.invalid".to_string(),
            scheme: "http".into(),
        },
        forgejo: ferry::git::askpass::Origin {
            host: "forgejo.invalid".to_string(),
            scheme: "http".into(),
        },
        forgejo_user: FORGEJO_LOGIN.to_string(),
        secrets: vec![Token::new(FORGEJO_TOKEN)],
        askpass_path: PathBuf::from(env!("CARGO_BIN_EXE_ferry")),
        git_program: PathBuf::from("git"),
        cancel,
    }
}

/// Creates `path` (and parents) as a bare repository whose default branch is `main`.
pub fn init_bare(path: &Path) {
    std::fs::create_dir_all(path).expect("create bare repo dir");
    git(
        path,
        &[
            "-c",
            "init.defaultBranch=main",
            "init",
            "--bare",
            "--quiet",
            ".",
        ],
    );
}

/// `refs/heads/*` and `refs/tags/*` of a bare repository, as ferry sees them.
pub fn ref_map(bare: &Path) -> RefMap {
    let text = git(
        bare,
        &[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/heads",
            "refs/tags",
        ],
    );
    ferry::git::refs::parse_for_each_ref(&text)
}

/// Deletes every branch of a bare repository, leaving tags.
pub fn delete_all_branches(bare: &Path) {
    let heads = git(bare, &["for-each-ref", "--format=%(refname)", "refs/heads"]);
    for head in heads.lines() {
        git(bare, &["update-ref", "-d", head]);
    }
}

/// A GitHub-side repository: a bare repository plus a scratch work tree.
pub struct SourceRepo {
    pub bare: PathBuf,
    pub work: PathBuf,
}

impl SourceRepo {
    fn git(&self, args: &[&str]) -> String {
        git(&self.work, args)
    }

    /// Commits `content` to `file` on the current branch and pushes it.
    pub fn commit(&self, file: &str, content: &str) -> String {
        std::fs::write(self.work.join(file), content).expect("write file");
        self.git(&["add", "--all"]);
        self.git(&["commit", "--quiet", "-m", &format!("update {file}")]);
        let branch = self.git(&["rev-parse", "--abbrev-ref", "HEAD"]);
        self.git(&["push", "--quiet", "origin", &branch]);
        self.git(&["rev-parse", "HEAD"])
    }

    /// Creates `branch` from the current commit and pushes it.
    pub fn branch(&self, branch: &str) {
        self.git(&["branch", "--force", branch]);
        self.git(&["push", "--quiet", "--force", "origin", branch]);
    }

    pub fn delete_branch(&self, branch: &str) {
        self.git(&["push", "--quiet", "origin", "--delete", branch]);
    }

    /// Force-pushes the local `tag` to origin.
    fn push_tag(&self, tag: &str) {
        self.git(&[
            "push",
            "--quiet",
            "--force",
            "origin",
            &format!("refs/tags/{tag}"),
        ]);
    }

    pub fn tag(&self, tag: &str) {
        self.git(&["tag", "--force", tag]);
        self.push_tag(tag);
    }

    pub fn annotated_tag(&self, tag: &str) {
        self.git(&["tag", "--force", "-a", "-m", tag, tag]);
        self.push_tag(tag);
    }

    pub fn delete_tag(&self, tag: &str) {
        self.git(&[
            "push",
            "--quiet",
            "origin",
            "--delete",
            &format!("refs/tags/{tag}"),
        ]);
    }

    /// Replaces the tip commit of the current branch and force-pushes: the
    /// old tip is no longer an ancestor of the new one.
    pub fn rewrite_tip(&self, file: &str, content: &str) -> String {
        std::fs::write(self.work.join(file), content).expect("write file");
        self.git(&["add", "--all"]);
        self.git(&["commit", "--quiet", "--amend", "-m", "rewritten"]);
        let branch = self.git(&["rev-parse", "--abbrev-ref", "HEAD"]);
        self.git(&["push", "--quiet", "--force", "origin", &branch]);
        self.git(&["rev-parse", "HEAD"])
    }

    /// Renames the default branch the way GitHub does: the new branch
    /// appears, `HEAD` moves to it, and the old branch is gone.
    pub fn rename_default_branch(&self, from: &str, to: &str) {
        self.git(&["branch", "--move", from, to]);
        self.git(&["push", "--quiet", "origin", to]);
        git(
            &self.bare,
            &["symbolic-ref", "HEAD", &format!("refs/heads/{to}")],
        );
        self.git(&["push", "--quiet", "origin", "--delete", from]);
    }

    /// Deletes every branch, leaving tags: GitHub "reports zero branches".
    pub fn delete_all_branches(&self) {
        delete_all_branches(&self.bare);
    }

    pub fn refs(&self) -> RefMap {
        ref_map(&self.bare)
    }
}
