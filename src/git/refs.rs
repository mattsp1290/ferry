//! Ref maps and `git ls-remote` parsing.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

const HEADS: &str = "refs/heads/";
const TAGS: &str = "refs/tags/";

/// Refname to object id, sorted by refname.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefMap(BTreeMap<String, String>);

impl RefMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, refname: impl Into<String>, oid: impl Into<String>) {
        self.0.insert(refname.into(), oid.into());
    }

    pub fn get(&self, refname: &str) -> Option<&str> {
        self.0.get(refname).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// All refs, sorted by refname.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(name, oid)| (name.as_str(), oid.as_str()))
    }

    /// Only `refs/heads/*`.
    pub fn heads(&self) -> impl Iterator<Item = (&str, &str)> {
        self.iter().filter(|(name, _)| name.starts_with(HEADS))
    }

    pub fn has_heads(&self) -> bool {
        self.heads().next().is_some()
    }

    /// Number of refs in the symmetric difference: refs created, moved, or
    /// deleted when going from `self` to `other`.
    pub fn diff_count(&self, other: &Self) -> usize {
        let changed_or_deleted = self
            .0
            .iter()
            .filter(|(name, oid)| other.0.get(*name) != Some(*oid))
            .count();
        let created = other
            .0
            .keys()
            .filter(|name| !self.0.contains_key(*name))
            .count();
        changed_or_deleted + created
    }

    /// Refnames present in `self` but absent from `other`.
    pub fn missing_in(&self, other: &Self) -> Vec<String> {
        self.0
            .keys()
            .filter(|name| !other.0.contains_key(*name))
            .cloned()
            .collect()
    }

    /// Lowercase hex SHA-256 over the sorted `"<oid> <refname>\n"` lines.
    pub fn hash(&self) -> String {
        let mut hasher = Sha256::new();
        for (name, oid) in &self.0 {
            hasher.update(oid.as_bytes());
            hasher.update(b" ");
            hasher.update(name.as_bytes());
            hasher.update(b"\n");
        }
        hasher
            .finalize()
            .iter()
            .fold(String::with_capacity(64), |mut hex, byte| {
                hex.push_str(&format!("{byte:02x}"));
                hex
            })
    }
}

impl FromIterator<(String, String)> for RefMap {
    fn from_iter<T: IntoIterator<Item = (String, String)>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// What `git ls-remote` reports for a remote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteState {
    /// `refs/heads/*` and `refs/tags/*` only.
    pub refs: RefMap,
    /// Branch name that `HEAD` points at. `None` for an empty repository.
    pub head: Option<String>,
}

/// Parses `git ls-remote --symref <url> HEAD 'refs/heads/*' 'refs/tags/*'`.
///
/// Peeled `^{}` lines are dropped. `head` is `None` unless the named branch is
/// among the returned refs, which also covers the unborn `HEAD` that newer git
/// reports for an empty repository.
pub fn parse_ls_remote(text: &str) -> RemoteState {
    let mut refs = RefMap::new();
    let mut head_target: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("ref: ") {
            if let Some((target, "HEAD")) = rest.split_once('\t')
                && let Some(branch) = target.strip_prefix(HEADS)
            {
                head_target = Some(branch.to_string());
            }
            continue;
        }
        let Some((oid, name)) = line.split_once('\t') else {
            continue;
        };
        if name.ends_with("^{}") {
            continue;
        }
        if name.starts_with(HEADS) || name.starts_with(TAGS) {
            refs.insert(name, oid);
        }
    }
    let head = head_target.filter(|branch| refs.get(&format!("{HEADS}{branch}")).is_some());
    RemoteState { refs, head }
}

/// Parses `git for-each-ref --format='%(objectname) %(refname)'`.
pub fn parse_for_each_ref(text: &str) -> RefMap {
    text.lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(oid, name)| (name.to_string(), oid.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccc";

    fn map(entries: &[(&str, &str)]) -> RefMap {
        entries
            .iter()
            .map(|(n, o)| (n.to_string(), o.to_string()))
            .collect()
    }

    #[test]
    fn parses_symref_heads_tags_and_drops_peeled() {
        let text = format!(
            "ref: refs/heads/main\tHEAD\n{A}\tHEAD\n{A}\trefs/heads/main\n{B}\trefs/heads/dev\n\
             {C}\trefs/tags/v1\n{A}\trefs/tags/v1^{{}}\n"
        );
        let state = parse_ls_remote(&text);
        assert_eq!(state.head.as_deref(), Some("main"));
        assert_eq!(state.refs.len(), 3);
        assert_eq!(state.refs.get("refs/heads/dev"), Some(B));
        assert_eq!(state.refs.get("refs/tags/v1"), Some(C));
        assert!(state.refs.has_heads());
        assert_eq!(state.refs.heads().count(), 2);
    }

    #[test]
    fn unborn_head_of_empty_repository_is_none() {
        let state = parse_ls_remote("ref: refs/heads/main\tHEAD\n");
        assert_eq!(state, RemoteState::default());
        assert_eq!(parse_ls_remote(""), RemoteState::default());
    }

    #[test]
    fn head_is_none_when_branch_missing_from_refs() {
        let text = format!("ref: refs/heads/gone\tHEAD\n{A}\trefs/heads/main\n");
        assert_eq!(parse_ls_remote(&text).head, None);
    }

    #[test]
    fn ignores_refs_outside_heads_and_tags() {
        let text = format!("{A}\trefs/pull/1/head\n{A}\trefs/heads/main\n");
        assert_eq!(parse_ls_remote(&text).refs.len(), 1);
    }

    #[test]
    fn parses_for_each_ref() {
        let refs = parse_for_each_ref(&format!("{A} refs/heads/main\n{B} refs/tags/v1\n"));
        assert_eq!(refs, map(&[("refs/heads/main", A), ("refs/tags/v1", B)]));
    }

    #[test]
    fn diff_counts_created_moved_and_deleted() {
        let old = map(&[("refs/heads/a", A), ("refs/heads/b", A), ("refs/tags/t", A)]);
        let new = map(&[
            ("refs/heads/a", A),
            ("refs/heads/b", B),
            ("refs/heads/c", C),
        ]);
        // b moved, t deleted, c created.
        assert_eq!(old.diff_count(&new), 3);
        assert_eq!(new.diff_count(&old), 3);
        assert_eq!(old.diff_count(&old), 0);
        assert_eq!(old.missing_in(&new), vec!["refs/tags/t".to_string()]);
        assert_eq!(new.missing_in(&old), vec!["refs/heads/c".to_string()]);
    }

    #[test]
    fn hash_is_sha256_of_sorted_lines() {
        // Inserted out of order; the hash covers sorted lines.
        let refs = map(&[("refs/tags/v1", B), ("refs/heads/main", A)]);
        let mut hasher = Sha256::new();
        hasher.update(format!("{A} refs/heads/main\n{B} refs/tags/v1\n").as_bytes());
        let expected: String = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(refs.hash(), expected);
        assert_eq!(refs.hash().len(), 64);
        // Empty map hashes the empty string.
        assert_eq!(
            RefMap::new().hash(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
