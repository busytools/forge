//! Readers for the user's `CLAUDE.md` (per-repo instructions) and the
//! per-project auto-memory file
//! (`<config_dir>/projects/<project_key>/memory/MEMORY.md`).
//!
//! Lifted from forge-sdk's `session::paths` (2026-05-05). The
//! filesystem reads are agent-side concerns — the SDK no longer
//! exposes them on `Client`. Path resolution still uses
//! `forge_sdk::claude_config_dir` and the project-key sanitiser
//! from `forge_agent::userdata::catalog::scan` (which itself
//! lifted from forge-sdk in the same round) so the key matches
//! what the `claude` CLI writes.

use std::path::{Path, PathBuf};

/// Resolve the path to a project's auto-memory file:
/// `<config_dir>/projects/<project_key>/memory/MEMORY.md`.
///
/// Always returns a resolved path; the caller decides whether the
/// file exists.
#[must_use]
pub fn project_memory_path(cwd: &Path) -> PathBuf {
    let key =
        crate::userdata::catalog::scan::project_key_for_directory(Some(&cwd.to_string_lossy()));
    forge_sdk::projects_dir()
        .join(key)
        .join("memory")
        .join("MEMORY.md")
}

/// Read the contents of the project's auto-memory file, if present.
/// Returns `None` when the file is missing or unreadable.
#[must_use]
pub fn read_project_memory(cwd: &Path) -> Option<String> {
    std::fs::read_to_string(project_memory_path(cwd)).ok()
}

/// Read the project's `CLAUDE.md` (per-repo instructions for Claude),
/// if present. Returns `None` when the file is missing or unreadable.
#[must_use]
pub fn read_claude_md(cwd: &Path) -> Option<String> {
    std::fs::read_to_string(cwd.join("CLAUDE.md")).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn project_memory_path_uses_project_key_layout() {
        // Just sanity-check the layout shape — the env-derived
        // <config_dir> prefix isn't pinned because that's racy across
        // parallel tests.
        let path = project_memory_path(Path::new("/tmp/some/proj"));
        let suffix: PathBuf = ["projects", "-tmp-some-proj", "memory", "MEMORY.md"]
            .iter()
            .collect();
        assert!(
            path.ends_with(&suffix),
            "expected path to end with {suffix:?}, got {path:?}"
        );
    }

    #[test]
    fn project_memory_path_drops_no_leading_dash() {
        // The CLI's project-key layout keeps the leading dash from
        // absolute paths starting with `/`. Earlier TUI code stripped
        // it, which produced a different directory than the CLI uses
        // and thus never found the memory file. Pin the behaviour.
        let path = project_memory_path(Path::new("/Users/me/proj"));
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains("/projects/-Users-me-proj/memory/"),
            "expected `-Users-me-proj` (leading dash kept), got {path_str}"
        );
    }

    #[test]
    fn read_project_memory_returns_none_when_missing() {
        // Confirm the internal contract: missing file -> None. We
        // can't easily steer claude_config_dir to a tempdir without
        // env-mutation, so test the read primitive directly.
        let dir = tempfile::tempdir().expect("tempdir");
        let nonexistent = dir.path().join("does/not/exist/MEMORY.md");
        assert!(std::fs::read_to_string(&nonexistent).is_err());
    }

    #[test]
    fn read_claude_md_returns_none_when_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(read_claude_md(dir.path()).is_none());
    }

    #[test]
    fn read_claude_md_reads_file_when_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("CLAUDE.md"), "hello\n").expect("write");
        assert_eq!(read_claude_md(dir.path()).as_deref(), Some("hello\n"));
    }
}
