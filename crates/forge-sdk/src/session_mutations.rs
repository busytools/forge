//! Filesystem-backed session mutations. Ports the public entry points of
//! Python SDK v0.1.64 `_internal/session_mutations.py`:
//!
//! - [`rename_session`] — appends a `custom-title` JSONL entry.
//! - [`tag_session`] — appends a `tag` JSONL entry (pass `None` to clear).
//! - [`delete_session`] — removes the `<session_id>.jsonl` file and any
//!   sibling `<session_id>/` subagent-transcript directory.
//!
//! Not yet ported: `fork_session` / `ForkSessionResult` (full transcript
//! walk + UUID remapping) and the `*_via_store` async variants. These
//! are follow-ups; the ones above cover the bulk of typical use.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::error::Error;

/// Rename a session by appending a `custom-title` entry. Calls
/// `list_sessions` / `get_session_info` will surface the most recently
/// appended title.
///
/// # Errors
///
/// - [`Error::MessageParse`] when `session_id` is not a valid UUID or
///   `title` is empty after trimming.
/// - [`Error::Io`] when the session file can't be found or the append
///   fails.
pub fn rename_session(session_id: &str, title: &str, directory: Option<&str>) -> Result<(), Error> {
    validate_uuid(session_id)?;
    let stripped = title.trim();
    if stripped.is_empty() {
        return Err(Error::MessageParse {
            reason: "title must be non-empty".into(),
        });
    }
    let payload = json!({
        "type": "custom-title",
        "customTitle": stripped,
        "sessionId": session_id,
    });
    append_to_session(session_id, directory, &payload)
}

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
                return Err(Error::MessageParse {
                    reason: "tag must be non-empty (use None to clear)".into(),
                });
            }
            stripped.to_string()
        }
    };
    let payload = json!({
        "type": "tag",
        "tag": stored,
        "sessionId": session_id,
    });
    append_to_session(session_id, directory, &payload)
}

/// Delete a session — removes the `<session_id>.jsonl` file and any
/// sibling `<session_id>/` subdirectory that holds subagent transcripts.
///
/// # Errors
///
/// - [`Error::MessageParse`] when `session_id` is not a valid UUID.
/// - [`Error::Io`] when the session file can't be found or removal fails.
pub fn delete_session(session_id: &str, directory: Option<&str>) -> Result<(), Error> {
    validate_uuid(session_id)?;
    let path = find_session_file(session_id, directory).ok_or_else(|| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("session {session_id} not found"),
        ))
    })?;
    fs::remove_file(&path)?;
    // Sibling subagents directory — best-effort cleanup.
    if let Some(parent) = path.parent() {
        let _ = fs::remove_dir_all(parent.join(session_id));
    }
    Ok(())
}

fn validate_uuid(s: &str) -> Result<(), Error> {
    let parts: Vec<&str> = s.split('-').collect();
    let ok = parts.len() == 5
        && matches!(
            (
                parts[0].len(),
                parts[1].len(),
                parts[2].len(),
                parts[3].len(),
                parts[4].len()
            ),
            (8, 4, 4, 4, 12)
        )
        && parts
            .iter()
            .all(|p| p.chars().all(|c| c.is_ascii_hexdigit()));
    if ok {
        Ok(())
    } else {
        Err(Error::MessageParse {
            reason: format!("Invalid session_id: {s}"),
        })
    }
}

fn projects_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(custom.trim_end_matches('/')).join("projects");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".claude").join("projects")
}

fn find_session_file(session_id: &str, directory: Option<&str>) -> Option<PathBuf> {
    let file_name = format!("{session_id}.jsonl");
    if let Some(dir) = directory {
        let canonical = match fs::canonicalize(dir) {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(_) => dir.to_string(),
        };
        let project_dir = projects_dir().join(crate::sessions::sanitize_path_public(&canonical));
        let candidate = project_dir.join(&file_name);
        return candidate.is_file().then_some(candidate);
    }
    fs::read_dir(projects_dir()).ok().and_then(|iter| {
        iter.flatten()
            .map(|e| e.path().join(&file_name))
            .find(|p| p.is_file())
    })
}

fn append_to_session(
    session_id: &str,
    directory: Option<&str>,
    payload: &serde_json::Value,
) -> Result<(), Error> {
    let path = find_session_file(session_id, directory).ok_or_else(|| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("session {session_id} not found"),
        ))
    })?;
    let mut line = serde_json::to_string(payload).map_err(|e| Error::MessageParse {
        reason: format!("encode mutation payload: {e}"),
    })?;
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
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn invalid_uuid_is_rejected_on_rename() {
        let r = rename_session("not-a-uuid", "title", None);
        assert!(matches!(r, Err(Error::MessageParse { .. })));
    }

    #[test]
    fn empty_title_is_rejected() {
        let session_id = "550e8400-e29b-41d4-a716-446655440000";
        let r = rename_session(session_id, "   ", None);
        assert!(matches!(r, Err(Error::MessageParse { .. })));
    }

    #[test]
    fn empty_tag_is_rejected() {
        let session_id = "550e8400-e29b-41d4-a716-446655440001";
        let r = tag_session(session_id, Some("   "), None);
        assert!(matches!(r, Err(Error::MessageParse { .. })));
    }

    #[test]
    fn invalid_uuid_is_rejected_on_delete() {
        let r = delete_session("not-a-uuid", None);
        assert!(matches!(r, Err(Error::MessageParse { .. })));
    }
}
