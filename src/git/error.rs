//! Git failure type, stderr sanitising, and error classification.

use std::fmt;

use crate::config::Token;
use crate::util::truncate_at_char_boundary;

/// Captured stderr is cut to this many bytes before it enters an error.
pub const STDERR_LIMIT: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GitErrorKind {
    Timeout,
    Cancelled,
    Auth,
    NotFound,
    Rejected,
    Network,
    Other,
}

impl GitErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Auth => "auth",
            Self::NotFound => "not_found",
            Self::Rejected => "rejected",
            Self::Network => "network",
            Self::Other => "other",
        }
    }
}

/// A failed git operation. `stderr` is redacted and truncated, so `Display`
/// is safe to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitError {
    pub kind: GitErrorKind,
    pub operation: &'static str,
    pub exit_code: Option<i32>,
    pub stderr: String,
}

impl GitError {
    pub(super) fn other(operation: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind: GitErrorKind::Other,
            operation,
            exit_code: None,
            stderr: message.into(),
        }
    }
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "git {} failed ({}", self.operation, self.kind.as_str())?;
        if let Some(code) = self.exit_code {
            write!(f, ", exit code {code}")?;
        }
        f.write_str(")")?;
        let stderr = self.stderr.trim();
        if !stderr.is_empty() {
            write!(f, ": {stderr}")?;
        }
        Ok(())
    }
}

impl std::error::Error for GitError {}

/// Replaces every secret value with `[redacted]`, then truncates to
/// [`STDERR_LIMIT`] bytes on a char boundary. Redacting first means a token
/// that straddles the cut can never leak a prefix.
pub fn sanitize_stderr(raw: &[u8], secrets: &[Token]) -> String {
    let mut text = String::from_utf8_lossy(raw).into_owned();
    let mut values: Vec<&str> = secrets
        .iter()
        .map(Token::expose)
        .filter(|value| !value.is_empty())
        .collect();
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    for value in values {
        text = text.replace(value, "[redacted]");
    }
    truncate_at_char_boundary(&mut text, STDERR_LIMIT);
    text
}

/// Maps known stderr fragments to an error kind.
pub fn classify(stderr: &str) -> GitErrorKind {
    let has = |needles: &[&str]| needles.iter().any(|needle| stderr.contains(needle));
    if has(&[
        "Authentication failed",
        "terminal prompts disabled",
        "could not read Username",
        "could not read Password",
        "Invalid username or password",
        "error: 401",
        "error: 403",
    ]) {
        GitErrorKind::Auth
    } else if has(&[
        "Repository not found",
        "does not appear to be a git repository",
        "error: 404",
    ]) || (has(&["repository '"]) && has(&["not found"]))
    {
        GitErrorKind::NotFound
    } else if has(&[
        "[rejected]",
        "[remote rejected]",
        "pre-receive hook declined",
    ]) {
        GitErrorKind::Rejected
    } else if has(&[
        "Could not resolve host",
        "Connection",
        "Failed to connect",
        "Operation timed out",
    ]) {
        GitErrorKind::Network
    } else {
        GitErrorKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_redacts_tokens() {
        let secrets = [Token::new("test-token-not-real")];
        let text = sanitize_stderr(b"fatal: bad test-token-not-real here", &secrets);
        assert_eq!(text, "fatal: bad [redacted] here");
    }

    #[test]
    fn sanitize_redacts_before_truncating() {
        let token = "test-token-not-real";
        // Unredacted, the 4 KiB cut would fall inside the token.
        let mut raw = "x".repeat(STDERR_LIMIT - 12).into_bytes();
        raw.extend_from_slice(token.as_bytes());
        raw.extend_from_slice(b" tail");
        let text = sanitize_stderr(&raw, &[Token::new(token)]);
        assert!(text.len() <= STDERR_LIMIT);
        assert!(!text.contains("test-"), "prefix leaked");
        assert!(text.contains("[redacted]"));
    }

    #[test]
    fn sanitize_truncates_on_char_boundary() {
        let raw = "é".repeat(STDERR_LIMIT).into_bytes();
        let text = sanitize_stderr(&raw, &[]);
        assert!(text.len() <= STDERR_LIMIT);
        assert!(text.chars().all(|c| c == 'é'));
    }

    #[test]
    fn classifies_known_fragments() {
        let cases = [
            ("remote: Authentication failed for 'x'", GitErrorKind::Auth),
            (
                "fatal: could not read Username for 'https://h': terminal prompts disabled",
                GitErrorKind::Auth,
            ),
            ("remote: Repository not found.", GitErrorKind::NotFound),
            (
                "fatal: repository 'https://h/x' not found",
                GitErrorKind::NotFound,
            ),
            (
                " ! [rejected] main -> main (fetch first)",
                GitErrorKind::Rejected,
            ),
            (
                " ! [remote rejected] main (hook declined)",
                GitErrorKind::Rejected,
            ),
            ("fatal: Could not resolve host: nope", GitErrorKind::Network),
            ("curl: Connection refused", GitErrorKind::Network),
            ("fatal: something odd", GitErrorKind::Other),
        ];
        for (stderr, kind) in cases {
            assert_eq!(classify(stderr), kind, "{stderr}");
        }
    }

    #[test]
    fn display_is_single_summary_plus_stderr() {
        let error = GitError {
            kind: GitErrorKind::Rejected,
            operation: "push",
            exit_code: Some(1),
            stderr: "boom\n".into(),
        };
        assert_eq!(
            error.to_string(),
            "git push failed (rejected, exit code 1): boom"
        );
    }
}
