//! One working tree's files, and the ranking the `@` trigger reads a
//! query against. The walk itself is `forge_agent::env::file_index`'s;
//! this is the shape a view holds of it and the order it shows.

use std::collections::BTreeMap;
use std::path::Path;

pub use forge_workspace::env::file_index::FileCandidate;

/// The files under one root, keyed by their path relative to it.
#[derive(Default)]
pub struct FileIndex {
    pub entries: BTreeMap<String, FileCandidate>,
}

/// Whether a walk honours gitignore, from the CLI's per-user preferences
/// document: its `respectGitignore` key, absent or not a boolean reading
/// as `true`, which is the CLI's own default and what the terminal's `@`
/// list does. Both views read the key here, so neither can drift on how
/// it is read.
pub fn respect_gitignore(preferences: Option<&serde_json::Value>) -> bool {
    preferences
        .and_then(|document| document.get("respectGitignore"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
}

impl FileIndex {
    /// Walk `root` once and index what is under it. `honour_gitignore` is
    /// the reader's own preference, which the walker honours.
    pub fn scan(root: &Path, honour_gitignore: bool) -> Self {
        let entries =
            forge_workspace::env::file_index::collect_candidates(root, root, honour_gitignore)
                .into_iter()
                .map(|candidate| (candidate.rel_path.clone(), candidate))
                .collect();
        Self { entries }
    }

    /// The candidates `query` matches, best first, at most `limit` of
    /// them. The cap is the caller's: a dropdown and a page want
    /// different ones.
    pub fn visible(&self, query: &str, limit: usize) -> Vec<FileCandidate> {
        let query_lower = query.to_lowercase();
        let mut filtered: Vec<FileCandidate> = self
            .entries
            .values()
            .filter(|candidate| match_tier(candidate, &query_lower).is_some())
            .cloned()
            .collect();
        rank_and_truncate_candidates(&mut filtered, &query_lower, limit);
        filtered
    }
}

/// Best match first, then shallowest, then alphabetically, cut to
/// `limit`.
pub fn rank_and_truncate_candidates(
    candidates: &mut Vec<FileCandidate>,
    query_lower: &str,
    limit: usize,
) {
    candidates.sort_unstable_by(|a, b| {
        match_tier(a, query_lower)
            .cmp(&match_tier(b, query_lower))
            .then_with(|| a.depth.cmp(&b.depth))
            .then_with(|| a.rel_path.cmp(&b.rel_path))
    });
    candidates.truncate(limit);
}

/// How well `candidate` matches, smaller being better. An empty query
/// matches everything equally, which is what a bare `@` shows.
fn match_tier(candidate: &FileCandidate, query_lower: &str) -> Option<u8> {
    if query_lower.is_empty() {
        return Some(0);
    }

    if candidate.basename_lower.starts_with(query_lower) {
        Some(0)
    } else if candidate.rel_path_lower.starts_with(query_lower) {
        Some(1)
    } else if candidate.basename_lower.contains(query_lower) {
        Some(2)
    } else if candidate.rel_path_lower.contains(query_lower) {
        Some(3)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{FileCandidate, FileIndex, rank_and_truncate_candidates, respect_gitignore};

    fn candidate(rel_path: &str) -> FileCandidate {
        FileCandidate {
            rel_path: rel_path.to_owned(),
            rel_path_lower: rel_path.to_lowercase(),
            basename_lower: rel_path.rsplit('/').next().unwrap_or(rel_path).to_lowercase(),
            depth: rel_path.matches('/').count(),
        }
    }

    /// The preference is the document's own key, and the CLI's default is
    /// to respect the file: a reader with no document, or one that never
    /// set the key, hides ignored files.
    #[test]
    fn the_ignore_preference_is_the_documents_key_and_defaults_to_true() {
        use serde_json::json;

        assert!(respect_gitignore(None), "no document reads as the CLI's own default");
        assert!(respect_gitignore(Some(&json!({}))), "and so does one without the key");
        assert!(
            !respect_gitignore(Some(&json!({"respectGitignore": false}))),
            "the key is what the walk follows",
        );
        assert!(
            respect_gitignore(Some(&json!({"respectGitignore": "no"}))),
            "and a value that is not a boolean reads as the default rather than as off",
        );
    }

    /// The walk is recursive and keys every file by its path from the
    /// root, so a nested file is reachable by its own path rather than
    /// by its basename alone.
    #[test]
    fn a_scan_indexes_nested_files_under_their_relative_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        for file in ["top.rs", "src/nested.rs"] {
            let path = tmp.path().join(file);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(&path, "").expect("write");
        }

        let index = FileIndex::scan(tmp.path(), true);

        assert!(
            index.entries.contains_key("src/nested.rs"),
            "a nested file is indexed by its path: {:?}",
            index.entries.keys().collect::<Vec<_>>(),
        );
        assert!(index.entries.contains_key("top.rs"), "and so is a top-level one");
    }

    /// A basename prefix beats a substring that happens to sit in a
    /// shallower path, which is the whole reason the tiers exist.
    #[test]
    fn a_basename_prefix_ranks_ahead_of_a_shallow_path_substring() {
        let mut candidates = vec![candidate("docs/guide-rs.txt"), candidate("src/rs-helper.rs")];

        rank_and_truncate_candidates(&mut candidates, "rs", 10);

        assert_eq!(candidates[0].rel_path, "src/rs-helper.rs");
    }

    /// The query is matched case-insensitively, against the lowercased
    /// paths the walk already carries.
    #[test]
    fn a_query_matches_regardless_of_case() {
        let mut index = FileIndex::default();
        index.entries.insert("src/Main.rs".to_owned(), candidate("src/Main.rs"));

        let matched = index.visible("main", 10);

        assert_eq!(matched.len(), 1, "a capitalised path answers a lowercase query");
        assert_eq!(matched[0].rel_path, "src/Main.rs");
    }

    /// The cap is the caller's, and a caller that hands one over gets it.
    #[test]
    fn a_query_stops_at_the_callers_limit() {
        let mut index = FileIndex::default();
        for i in 0..5 {
            let path = format!("file{i}.rs");
            index.entries.insert(path.clone(), candidate(&path));
        }

        assert_eq!(index.visible("file", 2).len(), 2, "the limit is honoured");
        assert_eq!(index.visible("file", 50).len(), 5, "and nothing is dropped below it");
    }
}
