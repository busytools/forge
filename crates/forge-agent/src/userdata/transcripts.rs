//! Session transcripts on disk: the `.jsonl` files under the config
//! dir's `projects/` tree, each directory named for the path the
//! session ran in.

use crate::userdata::catalog::scan::{SessionTagCache, project_key_for_directory, tag_of};
use forge_primitives::worker_tag;
use forge_sdk::projects_dir_for;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Why a transcript lookup could not answer.
#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    /// The directory, or a transcript in it, is there but could not be
    /// read, so whether the label has a prior session is unknown. A
    /// directory that is simply absent is `Ok(None)` instead - nothing
    /// to resume is a fallback, while this is a failure: folded into the
    /// fallback it would tell the caller there is nothing to resume,
    /// which the lookup never answered.
    #[error("could not read {path}: {source}")]
    Unreadable { path: PathBuf, source: std::io::Error },
}

/// The session id of the newest transcript in `run_dir`'s directory that
/// carries `label`'s worker tag, or `None` when the directory holds none.
///
/// The tag is what says whose session a transcript is, not the
/// directory: everything that ran with that cwd writes there, the
/// worker's own subagents included, so the newest file in the directory
/// is often not the worker's.
pub fn newest_worker_session(
    config_dir: &Path,
    run_dir: &Path,
    label: &str,
    tag_cache: Option<&SessionTagCache>,
) -> Result<Option<String>, TranscriptError> {
    let key = project_key_for_directory(Some(&run_dir.to_string_lossy()));
    let dir = projects_dir_for(config_dir).join(key);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(TranscriptError::Unreadable { path: dir, source }),
    };
    let wanted = worker_tag(label);
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
        let metadata = entry
            .metadata()
            .map_err(|source| TranscriptError::Unreadable { path: path.clone(), source })?;
        let modified = metadata
            .modified()
            .map_err(|source| TranscriptError::Unreadable { path: path.clone(), source })?;
        // A file no newer than the best so far cannot win, and the tag
        // scan it would cost is the expensive part. The id breaks a tie,
        // so the winner does not depend on the order the directory
        // happens to list.
        let candidate = (modified, session_id);
        if newest.as_ref().is_some_and(|(best, id)| candidate <= (*best, id.as_str())) {
            continue;
        }
        if read_tag(&path, metadata.len(), tag_cache)?.as_deref() != Some(wanted.as_str()) {
            continue;
        }
        newest = Some((modified, session_id.to_owned()));
    }
    Ok(newest.map(|(_, session_id)| session_id))
}

/// The last tag the transcript at `path` carries.
fn read_tag(
    path: &Path,
    size: u64,
    tag_cache: Option<&SessionTagCache>,
) -> Result<Option<String>, TranscriptError> {
    let unreadable = |source| TranscriptError::Unreadable { path: path.to_path_buf(), source };
    let mut file = std::fs::File::open(path).map_err(unreadable)?;
    tag_of(path, &mut file, size, tag_cache).map_err(unreadable)
}

#[cfg(test)]
mod tests {

    use super::*;
    use std::ffi::OsStr;
    use std::io::Write as _;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn the_newest_session_tagged_for_the_label_wins() {
        let root = tempfile::tempdir().expect("config dir");
        let run_dir = Path::new("/p/.claude/worktrees/w");
        let dir = transcript_dir(root.path(), run_dir);
        for session_id in ["aaaa", "bbbb", "cccc"] {
            write_session(&dir, session_id, "steward", 1_700_000_000);
        }
        // The newest mtime goes to whichever id the directory lists in
        // the middle, where neither the first nor the last entry is: an
        // implementation that answered by listing order instead of by
        // time would name a different session here.
        let listed = session_ids(&dir);
        let newest = listed[1].clone();
        write_session(&dir, &newest, "steward", 1_800_000_000);
        let found = newest_worker_session(root.path(), run_dir, "steward", None).expect("lookup");
        assert_eq!(found, Some(newest), "the label's most recent session is the one to resume");
    }

    /// Everything that ran in this directory writes here, so the newest
    /// file is often not the worker's: a subagent of its own, or a
    /// session a person started by hand. The tag is what says whose
    /// history this is.
    #[test]
    fn a_newer_transcript_that_is_not_the_labels_is_not_resumed() {
        let root = tempfile::tempdir().expect("config dir");
        let run_dir = Path::new("/p/.claude/worktrees/w");
        let dir = transcript_dir(root.path(), run_dir);
        write_session(&dir, "mine", "steward", 1_700_000_000);
        write_session(&dir, "other-worker", "reviewer", 1_800_000_000);
        write_untagged_session(&dir, "untagged", 1_900_000_000);
        let found = newest_worker_session(root.path(), run_dir, "steward", None).expect("lookup");
        assert_eq!(found, Some("mine".to_owned()));
    }

    /// Two sessions can carry the same timestamp: a restored or synced
    /// tree keeps coarse mtimes, and a transcript being written now can
    /// share a second with the one before it. The id decides between
    /// them, so the answer does not depend on the order the directory
    /// happens to list.
    #[test]
    fn a_tie_on_mtime_is_broken_by_the_session_id() {
        let root = tempfile::tempdir().expect("config dir");
        let run_dir = Path::new("/p/.claude/worktrees/w");
        let dir = transcript_dir(root.path(), run_dir);
        for session_id in ["aaaa", "mmmm", "zzzz"] {
            write_session(&dir, session_id, "steward", 1_700_000_000);
        }
        let found = newest_worker_session(root.path(), run_dir, "steward", None).expect("lookup");
        assert_eq!(found, Some("zzzz".to_owned()), "the greater id wins an mtime tie");
    }

    #[test]
    fn a_transcript_that_is_not_a_session_is_not_resumed() {
        let root = tempfile::tempdir().expect("config dir");
        let run_dir = Path::new("/p/.claude/worktrees/w");
        let dir = transcript_dir(root.path(), run_dir);
        write_session(&dir, "mine", "steward", 1_700_000_000);
        // claude leaves these beside the sessions, they are newer than
        // the session they belong to, and they carry its tag.
        let tagged = "{\"type\":\"tag\",\"tag\":\"forge:worker:steward\"}\n";
        for sibling in ["other.jsonl.partial", "other.jsonl.bak"] {
            write_file(&dir.join(sibling), tagged, 1_900_000_000);
        }
        let found = newest_worker_session(root.path(), run_dir, "steward", None).expect("lookup");
        assert_eq!(found, Some("mine".to_owned()));
    }

    #[test]
    fn a_directory_that_is_not_there_is_absence_not_failure() {
        let root = tempfile::tempdir().expect("config dir");
        let found = newest_worker_session(
            root.path(),
            Path::new("/p/.claude/worktrees/never"),
            "steward",
            None,
        );
        assert_eq!(found.expect("a missing directory is not a failure"), None);
    }

    /// The label's transcript directory, created.
    fn transcript_dir(config_dir: &Path, run_dir: &Path) -> PathBuf {
        let key = crate::userdata::catalog::scan::project_key_for_directory(Some(
            &run_dir.to_string_lossy(),
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

    /// `<session_id>.jsonl` in `dir`, tagged for `label`, last written at
    /// `modified` (unix seconds).
    fn write_session(dir: &Path, session_id: &str, label: &str, modified: u64) {
        let tag = format!("{{\"type\":\"tag\",\"tag\":\"forge:worker:{label}\"}}\n");
        write_file(&dir.join(format!("{session_id}.jsonl")), &tag, modified);
    }

    /// A session that ran in this directory and carries no worker tag.
    fn write_untagged_session(dir: &Path, session_id: &str, modified: u64) {
        write_file(&dir.join(format!("{session_id}.jsonl")), "", modified);
    }

    fn write_file(path: &Path, body: &str, modified: u64) {
        let mut file = std::fs::File::create(path).expect("create transcript");
        file.write_all(format!("{{\"type\":\"user\"}}\n{body}").as_bytes())
            .expect("write transcript");
        file.set_modified(UNIX_EPOCH + Duration::from_secs(modified)).expect("set mtime");
    }
}
