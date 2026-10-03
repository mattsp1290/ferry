//! LFS-complete marker.
//!
//! After a verified sync of an `lfs = true` entry, ferry records the hash of
//! the mirrored ref map inside the cache repository. Equal ref maps on both
//! sides say nothing about LFS objects, so an entry whose marker is absent or
//! stale runs the LFS steps even when the refs already match. That covers an
//! entry switched to `lfs = true`, a lost cache, and a first LFS push that
//! failed.
//!
//! The marker is a performance aid only. Losing it costs one extra LFS pass.

use std::io;
use std::path::{Path, PathBuf};

/// File name of the marker inside the bare cache repository.
pub const MARKER_FILE: &str = "ferry-lfs-complete";

fn marker_path(cache_repo: &Path) -> PathBuf {
    cache_repo.join(MARKER_FILE)
}

/// The recorded ref-map hash, or `None` when no marker is readable.
pub async fn read(cache_repo: &Path) -> Option<String> {
    let text = tokio::fs::read_to_string(marker_path(cache_repo))
        .await
        .ok()?;
    let hash = text.trim();
    (!hash.is_empty()).then(|| hash.to_string())
}

/// Whether the marker records exactly `ref_map_hash`.
pub async fn is_current(cache_repo: &Path, ref_map_hash: &str) -> bool {
    read(cache_repo).await.as_deref() == Some(ref_map_hash)
}

/// Records `ref_map_hash`. Written to a temporary file and renamed, so a
/// crash never leaves a truncated marker that could match by accident.
pub async fn write(cache_repo: &Path, ref_map_hash: &str) -> io::Result<()> {
    let path = marker_path(cache_repo);
    let temporary = cache_repo.join(format!("{MARKER_FILE}.tmp"));
    tokio::fs::write(&temporary, format!("{ref_map_hash}\n")).await?;
    tokio::fs::rename(&temporary, &path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(read(dir.path()).await, None);
        assert!(!is_current(dir.path(), "abc").await);

        write(dir.path(), "abc").await.expect("write");
        assert_eq!(read(dir.path()).await.as_deref(), Some("abc"));
        assert!(is_current(dir.path(), "abc").await);
        assert!(!is_current(dir.path(), "def").await);

        write(dir.path(), "def").await.expect("overwrite");
        assert!(is_current(dir.path(), "def").await);
        assert!(!dir.path().join(format!("{MARKER_FILE}.tmp")).exists());
    }

    #[tokio::test]
    async fn empty_marker_is_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        tokio::fs::write(dir.path().join(MARKER_FILE), "\n")
            .await
            .expect("write");
        assert_eq!(read(dir.path()).await, None);
        assert!(!is_current(dir.path(), "").await);
    }

    #[tokio::test]
    async fn write_fails_without_the_cache_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(write(&dir.path().join("absent"), "abc").await.is_err());
    }
}
