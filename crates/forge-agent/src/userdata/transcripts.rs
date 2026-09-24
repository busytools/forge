//! Session transcripts on disk: the `.jsonl` files under the config
//! dir's `projects/` tree, each directory named for the path the
//! session ran in.

use crate::userdata::catalog::scan::project_key_for_directory;
use forge_sdk::projects_dir_for;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Why a transcript lookup could not answer.
#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    /// The directory is there but its contents could not be read, so
    /// whether it holds a prior session is unknown. A directory that is
    /// simply absent is `Ok(None)` instead - nothing to resume is a
    /// fallback, while this is a failure, and a caller that answered it
    /// with a fresh session would mint over a session it could not see.
    #[error("could not read the transcript directory {path}: {source}")]
    Unreadable { path: PathBuf, source: std::io::Error },
}

/// The session id of the most recently written transcript in
/// `worktree`'s directory, newest winning.
pub fn newest_session_for_worktree(
    config_dir: &Path,
    worktree: &Path,
) -> Result<Option<String>, TranscriptError> {
    let key = project_key_for_directory(Some(&worktree.to_string_lossy()));
    let dir = projects_dir_for(config_dir).join(key);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(TranscriptError::Unreadable { path: dir, source }),
    };
    let mut newest: Option<(SystemTime, String)> = None;
    for entry in entries {
        let entry =
            entry.map_err(|source| TranscriptError::Unreadable { path: dir.clone(), source })?;
        let path = entry.path();
        if path.extension().and_then(std::ffi::OsStr::to_str) != Some("jsonl") {
            continue;
        }
        let Some(session_id) = path.file_stem().and_then(std::ffi::OsStr::to_str) else {
            continue;
        };
        if session_id.is_empty() {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .map_err(|source| TranscriptError::Unreadable { path: path.clone(), source })?;
        // The id breaks a tie, so the winner does not depend on the
        // order the directory happens to list.
        let candidate = (modified, session_id.to_owned());
        if newest.as_ref().is_none_or(|current| candidate > *current) {
            newest = Some(candidate);
        }
    }
    Ok(newest.map(|(_, session_id)| session_id))
}

#[cfg(test)]
mod tests {

    use super::*;
    use std::ffi::OsStr;
    use std::io::Write as _;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn the_newest_session_in_a_worktree_wins() {
        let root = tempfile::tempdir().expect("config dir");
        let worktree = Path::new("/p/.claude/worktrees/w");
        let dir = transcript_dir(root.path(), worktree);
        for session_id in ["aaaa", "bbbb", "cccc"] {
            write_session(&dir, session_id, 1_700_000_000);
        }
        // The newest mtime goes to whichever id the directory lists in
        // the middle, where neither the first nor the last entry is: an
        // implementation that answered by listing order instead of by
        // time would name a different session here.
        let listed = session_ids(&dir);
        let newest = listed[1].clone();
        write_file(&dir.join(format!("{newest}.jsonl")), 1_800_000_000);
        let found = newest_session_for_worktree(root.path(), worktree).expect("lookup");
        assert_eq!(found, Some(newest), "the most recent session is the one to resume");
    }

    #[test]
    fn a_transcript_that_is_not_a_session_is_not_resumed() {
        let root = tempfile::tempdir().expect("config dir");
        let worktree = Path::new("/p/.claude/worktrees/w");
        let dir = transcript_dir(root.path(), worktree);
        write_session(&dir, "aaaa", 1_700_000_000);
        // claude leaves these beside the sessions, and they are newer
        // than the session they belong to.
        write_file(&dir.join("bbbb.jsonl.partial"), 1_900_000_000);
        let found = newest_session_for_worktree(root.path(), worktree).expect("lookup");
        assert_eq!(found, Some("aaaa".to_owned()));
    }

    #[test]
    fn a_worktree_with_no_sessions_is_absence_not_failure() {
        let root = tempfile::tempdir().expect("config dir");
        let found =
            newest_session_for_worktree(root.path(), Path::new("/p/.claude/worktrees/never"));
        assert_eq!(found.expect("a missing directory is not a failure"), None);
    }

    /// The label's transcript directory, created.
    fn transcript_dir(config_dir: &Path, worktree: &Path) -> PathBuf {
        let key = crate::userdata::catalog::scan::project_key_for_directory(Some(
            &worktree.to_string_lossy(),
        ));
        let dir = forge_sdk::projects_dir_for(config_dir).join(key);
        std::fs::create_dir_all(&dir).expect("transcript dir");
        dir
    }

    /// The `.jsonl` session ids in `dir`, in the order the directory
    /// lists them.
    fn session_ids(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .expect("read transcript dir")
            .map(|entry| entry.expect("dir entry").path())
            .filter(|path| path.extension().and_then(OsStr::to_str) == Some("jsonl"))
            .filter_map(|path| path.file_stem().and_then(OsStr::to_str).map(str::to_owned))
            .collect()
    }

    /// `<session_id>.jsonl` in `dir`, last written at `modified` (unix
    /// seconds).
    fn write_session(dir: &Path, session_id: &str, modified: u64) {
        write_file(&dir.join(format!("{session_id}.jsonl")), modified);
    }

    fn write_file(path: &Path, modified: u64) {
        let mut file = std::fs::File::create(path).expect("create transcript");
        file.write_all(b"{\"type\":\"user\"}\n").expect("write transcript");
        file.set_modified(UNIX_EPOCH + Duration::from_secs(modified)).expect("set mtime");
    }
}
