//! Token/cost accounting for the `/usage` view.
//!
//! [`pricing`] is the LiteLLM-sourced per-model USD table used to turn
//! JSONL token counts into a notional cost. This root holds the
//! project-folding that maps a `~/.claude/projects/<slug>` directory
//! name back to the repo it belongs to.

use std::path::{Path, PathBuf};

pub mod pricing;

/// Encoded form of `/.claude/worktrees/` in a project slug: Claude Code
/// maps both `/` and `.` to `-`, so a worktree path folds to
/// `<parent>--claude-worktrees-<name>`.
const WORKTREE_MARKER: &str = "--claude-worktrees-";

/// Fold a `~/.claude/projects/<slug>` directory name to the display
/// project it belongs to.
///
/// The slug is the project's absolute path with `/` and `.` both
/// replaced by `-`, so it is lossy: `trader-cc` and `trader/cc` encode
/// identically. Resolution therefore consults the filesystem (the
/// user's `~/Projects`) rather than splitting on `-`. Worktrees and
/// sub-paths fold to their repo, `/tmp` paths to `scratch`.
pub fn fold_project(slug: &str) -> String {
    let projects_root = home_dir().map(|home| home.join("Projects"));
    let prefix = projects_root.as_deref().map(encoded_projects_prefix).unwrap_or_default();
    let root = projects_root.as_deref().unwrap_or_else(|| Path::new(""));
    fold_project_in(slug, &prefix, root)
}

/// Testable core of [`fold_project`]. `projects_prefix` is the encoded
/// `<home>/Projects/` string to strip; an empty prefix disables the
/// repo-resolution rule (no known home). `projects_root` is the real
/// directory the candidate repo names are stat'd against.
fn fold_project_in(slug: &str, projects_prefix: &str, projects_root: &Path) -> String {
    // (a) A worktree folds to its parent repo; the part before the
    // marker is itself the parent's slug, so recurse on it.
    if let Some(idx) = slug.find(WORKTREE_MARKER) {
        return fold_project_in(&slug[..idx], projects_prefix, projects_root);
    }
    // (b) A path under <home>/Projects/: resolve the repo name against
    // the filesystem so a dashed name (`trader-cc`) isn't mis-split.
    if !projects_prefix.is_empty()
        && let Some(remainder) = slug.strip_prefix(projects_prefix)
        && !remainder.is_empty()
    {
        return resolve_project_name(remainder, projects_root);
    }
    // (c) /tmp and /private/tmp collapse into one scratch bucket.
    if slug.starts_with("-private-tmp") || slug.starts_with("-tmp") {
        return "scratch".to_owned();
    }
    // (d) Anything else: the trailing path component.
    basename_fallback(slug)
}

/// Resolve the repo name from a slug remainder (the encoded
/// `<name>/<subpath...>` after the `Projects/` prefix). Picks the
/// longest leading run of `-`-joined tokens that is an existing
/// directory under `projects_root`; when nothing resolves (the repo
/// was removed) the first component is the best-effort label.
fn resolve_project_name(remainder: &str, projects_root: &Path) -> String {
    let tokens: Vec<&str> = remainder.split('-').collect();
    for run_len in (1..=tokens.len()).rev() {
        let candidate = tokens[..run_len].join("-");
        if projects_root.join(&candidate).is_dir() {
            return candidate;
        }
    }
    tokens.first().map_or_else(|| remainder.to_owned(), |first| (*first).to_owned())
}

/// The trailing `-`-separated component of a slug, used when no richer
/// rule applies.
fn basename_fallback(slug: &str) -> String {
    slug.rsplit('-').find(|token| !token.is_empty()).unwrap_or(slug).to_owned()
}

/// The slug prefix for paths under `<projects_root>`: the root path with
/// `/` and `.` mapped to `-`, plus the trailing separator's `-`.
fn encoded_projects_prefix(projects_root: &Path) -> String {
    format!("{}-", projects_root.to_string_lossy().replace(['/', '.'], "-"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|s| !s.is_empty()).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const PREFIX: &str = "-Users-vedhavyas-Projects-";

    fn projects_root_with(dirs: &[&str]) -> TempDir {
        let td = tempfile::tempdir().expect("tempdir");
        for dir in dirs {
            std::fs::create_dir_all(td.path().join(dir)).expect("mkdir");
        }
        td
    }

    #[test]
    fn worktree_slug_folds_to_parent_repo() {
        let root = projects_root_with(&["busymail"]);
        let slug = "-Users-vedhavyas-Projects-busymail--claude-worktrees-abuse-hardening";
        assert_eq!(fold_project_in(slug, PREFIX, root.path()), "busymail");
    }

    #[test]
    fn sub_crate_slug_folds_to_repo_not_dash_split() {
        let root = projects_root_with(&["forge"]);
        let slug = "-Users-vedhavyas-Projects-forge-crates-forge-test-harness";
        assert_eq!(fold_project_in(slug, PREFIX, root.path()), "forge");
    }

    #[test]
    fn dashed_project_name_is_not_split_on_internal_dash() {
        let root = projects_root_with(&["trader-cc"]);
        let slug = "-Users-vedhavyas-Projects-trader-cc";
        assert_eq!(fold_project_in(slug, PREFIX, root.path()), "trader-cc");
    }

    #[test]
    fn tmp_slugs_fold_to_scratch() {
        let root = projects_root_with(&[]);
        assert_eq!(
            fold_project_in("-private-tmp-forge-refresh-0ed1d9d0", PREFIX, root.path()),
            "scratch",
        );
        assert_eq!(fold_project_in("-tmp-scratchpad", PREFIX, root.path()), "scratch");
    }

    #[test]
    fn tmp_worktree_slug_folds_to_scratch_via_parent() {
        let root = projects_root_with(&[]);
        let slug = "-private-tmp-claude-501--tmpoBGeeH--claude-worktrees-harness-spawn";
        assert_eq!(fold_project_in(slug, PREFIX, root.path()), "scratch");
    }

    #[test]
    fn vanished_repo_slug_falls_back_to_first_component() {
        // The repo dir is gone, so resolution can't confirm the name;
        // the leading path component is the best-effort repo label.
        let root = projects_root_with(&[]);
        let slug = "-Users-vedhavyas-Projects-ghostrepo-src-main";
        assert_eq!(fold_project_in(slug, PREFIX, root.path()), "ghostrepo");
    }
}
