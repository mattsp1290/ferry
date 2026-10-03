//! The result of one repository sync.
//!
//! The strings returned by `as_str` are metric tag values. Changing one is a
//! telemetry contract change: monitors and the dashboard query them.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyncResult {
    /// Refs or LFS objects were pushed and the destination was verified.
    Synced,
    /// Source and destination already matched.
    Noop,
    /// Both sides have no branches.
    Empty,
    Error,
}

impl SyncResult {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Synced => "synced",
            Self::Noop => "noop",
            Self::Empty => "empty",
            Self::Error => "error",
        }
    }

    /// Success for staleness purposes: every result except `Error`.
    pub fn is_success(self) -> bool {
        !matches!(self, Self::Error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    SourceMissing,
    SourceAuth,
    SourceEmpty,
    DestAuth,
    DestUnmanaged,
    DestIsPullMirror,
    DestRejected,
    Lfs,
    Metadata,
    VerifyMismatch,
    Timeout,
    Network,
    RateLimited,
    Internal,
}

impl ErrorKind {
    pub const ALL: [Self; 14] = [
        Self::SourceMissing,
        Self::SourceAuth,
        Self::SourceEmpty,
        Self::DestAuth,
        Self::DestUnmanaged,
        Self::DestIsPullMirror,
        Self::DestRejected,
        Self::Lfs,
        Self::Metadata,
        Self::VerifyMismatch,
        Self::Timeout,
        Self::Network,
        Self::RateLimited,
        Self::Internal,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SourceMissing => "source_missing",
            Self::SourceAuth => "source_auth",
            Self::SourceEmpty => "source_empty",
            Self::DestAuth => "dest_auth",
            Self::DestUnmanaged => "dest_unmanaged",
            Self::DestIsPullMirror => "dest_is_pull_mirror",
            Self::DestRejected => "dest_rejected",
            Self::Lfs => "lfs",
            Self::Metadata => "metadata",
            Self::VerifyMismatch => "verify_mismatch",
            Self::Timeout => "timeout",
            Self::Network => "network",
            Self::RateLimited => "rate_limited",
            Self::Internal => "internal",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOutcome {
    pub result: SyncResult,
    /// Set only when `result` is `Error`.
    pub error_kind: Option<ErrorKind>,
    /// Refs created, moved, or pruned on Forgejo.
    pub refs_changed: u32,
    /// Refs deleted on Forgejo.
    pub refs_pruned: u32,
    pub duration: Duration,
    /// For `rate_limited`: how long the forge asked ferry to wait.
    pub retry_after: Option<Duration>,
}

impl SyncOutcome {
    pub fn success(result: SyncResult, duration: Duration) -> Self {
        debug_assert!(result.is_success());
        Self {
            result,
            error_kind: None,
            refs_changed: 0,
            refs_pruned: 0,
            duration,
            retry_after: None,
        }
    }

    pub fn error(kind: ErrorKind, duration: Duration) -> Self {
        Self {
            result: SyncResult::Error,
            error_kind: Some(kind),
            refs_changed: 0,
            refs_pruned: 0,
            duration,
            retry_after: None,
        }
    }

    /// The `error_kind` tag value: the kind, or `none` when not an error.
    pub fn error_kind_tag(&self) -> &'static str {
        self.error_kind.map_or("none", ErrorKind::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_kind_strings_are_unique_snake_case() {
        let mut seen = std::collections::HashSet::new();
        for kind in ErrorKind::ALL {
            let name = kind.as_str();
            assert!(seen.insert(name), "duplicate error kind {name}");
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{name} is not snake_case"
            );
        }
    }

    #[test]
    fn error_kind_tag_is_none_for_success() {
        let outcome = SyncOutcome::success(SyncResult::Noop, Duration::ZERO);
        assert_eq!(outcome.error_kind_tag(), "none");
        let outcome = SyncOutcome::error(ErrorKind::Lfs, Duration::ZERO);
        assert_eq!(outcome.error_kind_tag(), "lfs");
        assert!(!outcome.result.is_success());
    }
}
