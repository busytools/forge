//! Readers for the user's `CLAUDE.md` (per-repo instructions) and the
//! per-project auto-memory file
//! (`<config_dir>/projects/<project_key>/memory/MEMORY.md`).
//!
//! The caller (typically a `ForgeSdkBridge`) threads the resolved
//! `config_dir` in; the project-key sanitiser from
//! `forge_agent::userdata::catalog::scan` keeps the on-disk layout
//! aligned with what the `claude` CLI writes.

use std::path::{Path, PathBuf};

/// Resolve the path to a project's auto-memory file:
/// `<config_dir>/projects/<project_key>/memory/MEMORY.md`.
///
/// Always returns a resolved path; the caller decides whether the
/// file exists.
pub fn project_memory_path(config_dir: &Path, cwd: &Path) -> PathBuf {
    let key =
        crate::userdata::catalog::scan::project_key_for_directory(Some(&cwd.to_string_lossy()));
    forge_sdk::projects_dir_for(config_dir).join(key).join("memory").join("MEMORY.md")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn project_memory_path_uses_project_key_layout() {
        let config_dir = PathBuf::from("/tmp/forge_test_memory_cfg");
        let path = project_memory_path(&config_dir, Path::new("/tmp/some/proj"));
        let expected: PathBuf =
            ["projects", "-tmp-some-proj", "memory", "MEMORY.md"].iter().collect();
        let expected_full = config_dir.join(expected);
        assert_eq!(path, expected_full);
    }

    #[test]
    fn project_memory_path_drops_no_leading_dash() {
        // The CLI's project-key layout keeps the leading dash from
        // absolute paths starting with `/`. Earlier TUI code stripped
        // it, which produced a different directory than the CLI uses
        // and thus never found the memory file. Pin the behaviour.
        let config_dir = PathBuf::from("/tmp/forge_test_memory_cfg2");
        let path = project_memory_path(&config_dir, Path::new("/Users/me/proj"));
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains("/projects/-Users-me-proj/memory/"),
            "expected `-Users-me-proj` (leading dash kept), got {path_str}"
        );
    }
}
