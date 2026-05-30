//! Filesystem-backed session mutations:
//!
//! - [`tag_session`] - appends a `tag` JSONL entry (pass `None` to clear).
//! - [`delete_session`] - removes the `<session_id>.jsonl` file and any
//!   sibling `<session_id>/` subagent-transcript directory.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::json;

use forge_sdk::{Error, projects_dir_for};

use crate::userdata::catalog::scan::is_valid_uuid;

/// Tag a session. Pass `None` to clear the tag (appends an empty-string
/// entry which `list_sessions` interprets as cleared).
///
/// # Errors
///
/// - [`Error::MessageParse`] when `session_id` is not a valid UUID or a
///   non-None `tag` is empty after trimming.
/// - [`Error::Io`] when the session file can't be found or the append
///   fails.
pub fn tag_session(
    config_dir: &Path,
    session_id: &str,
    tag: Option<&str>,
    directory: Option<&str>,
) -> Result<(), Error> {
    validate_uuid(session_id)?;
    let stored: String = match tag {
        None => String::new(),
        Some(raw) => {
            let stripped = raw.trim();
            if stripped.is_empty() {
                return Err(Error::message_parse("tag must be non-empty (use None to clear)"));
            }
            stripped.to_string()
        }
    };
    let payload = json!({
        "type": "tag",
        "tag": stored,
        "sessionId": session_id,
    });
    append_to_session(config_dir, session_id, directory, &payload)
}

/// Delete a session - removes the `<session_id>.jsonl` file and any
/// sibling `<session_id>/` subdirectory that holds subagent transcripts.
///
/// # Errors
///
/// - [`Error::MessageParse`] when `session_id` is not a valid UUID.
/// - [`Error::Io`] when the session file can't be found or removal fails.
pub fn delete_session(
    config_dir: &Path,
    session_id: &str,
    directory: Option<&str>,
) -> Result<(), Error> {
    validate_uuid(session_id)?;
    let path = find_session_file(config_dir, session_id, directory).ok_or_else(|| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("session {session_id} not found"),
        ))
    })?;
    fs::remove_file(&path)?;
    // Sibling subagents directory - best-effort cleanup. NotFound is
    // fine (no subagents ran); other errors leave orphaned transcripts
    // on disk and warrant a visible log so the user knows why
    // `list_subagents` will keep returning phantom entries.
    if let Some(parent) = path.parent() {
        let subagents = parent.join(session_id);
        if let Err(e) = fs::remove_dir_all(&subagents)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %subagents.display(),
                error = %e,
                "failed to clean up sibling subagents directory"
            );
        }
    }
    Ok(())
}

fn validate_uuid(s: &str) -> Result<(), Error> {
    if is_valid_uuid(s) {
        Ok(())
    } else {
        Err(Error::message_parse(format!("Invalid session_id: {s}")))
    }
}

fn find_session_file(
    config_dir: &Path,
    session_id: &str,
    directory: Option<&str>,
) -> Option<PathBuf> {
    let file_name = format!("{session_id}.jsonl");
    if let Some(dir) = directory {
        let canonical = match fs::canonicalize(dir) {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(_) => dir.to_string(),
        };
        let project_dir = projects_dir_for(config_dir)
            .join(crate::userdata::catalog::scan::sanitize_path_public(&canonical));
        let candidate = project_dir.join(&file_name);
        return candidate.is_file().then_some(candidate);
    }
    fs::read_dir(projects_dir_for(config_dir))
        .ok()
        .and_then(|iter| iter.flatten().map(|e| e.path().join(&file_name)).find(|p| p.is_file()))
}

fn append_to_session(
    config_dir: &Path,
    session_id: &str,
    directory: Option<&str>,
    payload: &serde_json::Value,
) -> Result<(), Error> {
    let path = find_session_file(config_dir, session_id, directory).ok_or_else(|| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("session {session_id} not found"),
        ))
    })?;
    let mut line =
        serde_json::to_string(payload).map_err(|e| Error::encode("mutation payload", e))?;
    line.push('\n');
    append_line(&path, line.as_bytes())
}

fn append_line(path: &Path, data: &[u8]) -> Result<(), Error> {
    let mut file = fs::OpenOptions::new().append(true).open(path)?;
    file.write_all(data)?;
    Ok(())
}

#[cfg(test)]
mod tests {

    use super::*;

    fn fake_config_dir() -> PathBuf {
        PathBuf::from("/tmp/forge_mutations_test_cfg")
    }

    #[test]
    fn empty_tag_is_rejected() {
        let session_id = "550e8400-e29b-41d4-a716-446655440001";
        let r = tag_session(&fake_config_dir(), session_id, Some("   "), None);
        assert!(matches!(r, Err(Error::MessageParse { .. })));
    }

    #[test]
    fn invalid_uuid_is_rejected_on_delete() {
        let r = delete_session(&fake_config_dir(), "not-a-uuid", None);
        assert!(matches!(r, Err(Error::MessageParse { .. })));
    }
}
