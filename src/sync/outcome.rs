//! The result of one repository sync.
//!
//! The strings returned by `result_tag`, `error_kind_tag`, and `ErrorKind::as_str`
//! are metric tag values. Changing one is a
//! telemetry contract change: monitors and the dashboard query them.

use std::time::Duration;

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

/// Status of a completed pass. Failure details and ref counts belong only
/// to the variants that can produce them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncStatus {
    /// Refs or LFS objects were pushed and the destination was verified.
    Synced {
        /// Refs created, moved, or pruned on Forgejo.
        refs_changed: u32,
        /// Refs deleted on Forgejo.
        refs_pruned: u32,
    },
    /// Source and destination already matched.
    Noop,
    /// Both sides have no branches.
    Empty,
    Failed {
        kind: ErrorKind,
        /// How long the forge asked Ferry to wait after a rate limit.
        retry_after: Option<Duration>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOutcome {
    pub status: SyncStatus,
    pub duration: Duration,
}

impl SyncOutcome {
    pub fn result_tag(&self) -> &'static str {
        match self.status {
            SyncStatus::Synced { .. } => "synced",
            SyncStatus::Noop => "noop",
            SyncStatus::Empty => "empty",
            SyncStatus::Failed { .. } => "error",
        }
    }

    pub fn error_kind_tag(&self) -> &'static str {
        match self.status {
            SyncStatus::Failed { kind, .. } => kind.as_str(),
            _ => "none",
        }
    }

    pub fn is_success(&self) -> bool {
        !matches!(self.status, SyncStatus::Failed { .. })
    }

    /// `(changed, pruned)`, or zero counts for a pass that did not sync refs.
    pub fn refs(&self) -> (u32, u32) {
        match self.status {
            SyncStatus::Synced {
                refs_changed,
                refs_pruned,
            } => (refs_changed, refs_pruned),
            _ => (0, 0),
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self.status {
            SyncStatus::Failed { retry_after, .. } => retry_after,
            _ => None,
        }
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
            assert!(name.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
        }
    }

    #[test]
    fn outcomes_preserve_metric_tags_and_variant_invariants() {
        let cases = [
            (
                SyncStatus::Synced {
                    refs_changed: 3,
                    refs_pruned: 1,
                },
                "synced",
                "none",
                (3, 1),
                None,
            ),
            (SyncStatus::Noop, "noop", "none", (0, 0), None),
            (SyncStatus::Empty, "empty", "none", (0, 0), None),
            (
                SyncStatus::Failed {
                    kind: ErrorKind::RateLimited,
                    retry_after: Some(Duration::from_secs(5)),
                },
                "error",
                "rate_limited",
                (0, 0),
                Some(Duration::from_secs(5)),
            ),
        ];
        for (status, result, error, refs, retry) in cases {
            let outcome = SyncOutcome {
                status,
                duration: Duration::ZERO,
            };
            assert_eq!(outcome.result_tag(), result);
            assert_eq!(outcome.error_kind_tag(), error);
            assert_eq!(outcome.refs(), refs);
            assert_eq!(outcome.retry_after(), retry);
            assert_eq!(outcome.is_success(), result != "error");
        }
    }
}
