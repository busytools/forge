//! Filesystem-backed session mutations. Ports the public entry points of
//! Python SDK v0.1.64 `_internal/session_mutations.py`:
//!
//! - [`rename_session`] — appends a `custom-title` JSONL entry.
//! - [`tag_session`] — appends a `tag` JSONL entry (pass `None` to clear).
//! - [`delete_session`] — removes the `<session_id>.jsonl` file and any
//!   sibling `<session_id>/` subagent-transcript directory.
//! - [`fork_session`] — copies transcript entries into a new session,
//!   remapping UUIDs. Optionally truncates at a supplied
//!   `up_to_message_id` boundary.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::Error;

/// Outcome of a [`fork_session`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkSessionResult {
    /// UUID of the new forked session.
    pub session_id: String,
}

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
        return Err(Error::message_parse("title must be non-empty"));
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
                return Err(Error::message_parse(
                    "tag must be non-empty (use None to clear)",
                ));
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
    // Sibling subagents directory — best-effort cleanup. NotFound is
    // fine (no subagents ran); other errors leave orphaned transcripts
    // on disk and warrant a visible log so the user knows why
    // `list_subagents` will keep returning phantom entries.
    if let Some(parent) = path.parent() {
        let subagents = parent.join(session_id);
        if let Err(e) = fs::remove_dir_all(&subagents) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    path = %subagents.display(),
                    error = %e,
                    "failed to clean up sibling subagents directory"
                );
            }
        }
    }
    Ok(())
}

/// Fork a session into a new branch. Copies the transcript line-by-line
/// into a new `<new_session_id>.jsonl` file, remapping every `uuid` /
/// `parentUuid` / `sessionId` field. When `up_to_message_id` is set,
/// stops copying after that message's UUID has been emitted.
///
/// # Errors
///
/// - [`Error::MessageParse`] when either UUID is invalid.
/// - [`Error::Io`] when the source file can't be found, is empty, or
///   the write fails.
#[allow(clippy::too_many_lines)]
pub fn fork_session(
    session_id: &str,
    directory: Option<&str>,
    up_to_message_id: Option<&str>,
    title: Option<&str>,
) -> Result<ForkSessionResult, Error> {
    validate_uuid(session_id)?;
    if let Some(m) = up_to_message_id {
        validate_uuid(m)?;
    }
    let source = find_session_file(session_id, directory).ok_or_else(|| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("session {session_id} not found"),
        ))
    })?;

    // Generate new session id up front so we can remap sessionId fields
    // inline as we scan.
    let new_session_id = Uuid::new_v4().to_string();
    let project_dir = source
        .parent()
        .ok_or_else(|| Error::Io(std::io::Error::other("session file missing parent dir")))?;
    let fork_path = project_dir.join(format!("{new_session_id}.jsonl"));
    if fork_path.exists() {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("fork target {new_session_id} already exists"),
        )));
    }

    // Read the whole transcript into memory first. Two passes are
    // unavoidable: if a child references a parent not yet seen, the
    // naive streaming approach leaves the parentUuid pointing at a UUID
    // that doesn't exist in the forked transcript.
    let mut raw_lines: Vec<String> = Vec::new();
    for line in BufReader::new(fs::File::open(&source)?)
        .lines()
        .map_while(Result::ok)
    {
        raw_lines.push(line);
    }

    // Pass 1 — mint a new UUID for every entry that has one, so
    // parentUuid references always find a mapping.
    let mut uuid_remap: HashMap<String, String> = HashMap::new();
    for (idx, line) in raw_lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(value) => {
                if let Some(old) = value.get("uuid").and_then(Value::as_str) {
                    uuid_remap
                        .entry(old.to_string())
                        .or_insert_with(|| Uuid::new_v4().to_string());
                }
            }
            Err(e) => {
                tracing::debug!(
                    line_no = idx,
                    error = %e,
                    "fork pass 1: skipping unparseable line; pass 2 will copy it verbatim"
                );
            }
        }
    }

    // Pass 2 — rewrite each entry using the fully-populated map.
    let mut out_lines: Vec<String> = Vec::new();
    let mut saw_boundary = false;

    for line in raw_lines {
        if line.is_empty() {
            continue;
        }
        let Ok(mut value) = serde_json::from_str::<Value>(&line) else {
            // Pass unparseable lines through verbatim — tolerant copy.
            out_lines.push(line);
            continue;
        };
        let boundary_hit =
            remap_entry_fields(&mut value, &uuid_remap, &new_session_id, up_to_message_id);
        if boundary_hit {
            saw_boundary = true;
        }
        out_lines.push(
            serde_json::to_string(&value).map_err(|e| Error::encode("fork session entry", e))?,
        );
        if saw_boundary {
            break;
        }
    }

    if out_lines.is_empty() {
        return Err(Error::Io(std::io::Error::other(format!(
            "session {session_id} has no messages to fork"
        ))));
    }
    if up_to_message_id.is_some() && !saw_boundary {
        return Err(Error::message_parse(format!(
            "up_to_message_id {} not found in transcript",
            up_to_message_id.unwrap_or("")
        )));
    }

    // Apply fork title — user-supplied wins, else derive from the source
    // (last customTitle / aiTitle / first prompt) and append " (fork)".
    let resolved_title = title
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .or_else(|| derive_fork_title(&source).map(|orig| format!("{orig} (fork)")));
    if let Some(final_title) = resolved_title {
        let title_entry = serde_json::to_string(&json!({
            "type": "custom-title",
            "customTitle": final_title,
            "sessionId": new_session_id,
        }))
        .map_err(|e| Error::encode("fork session title", e))?;
        out_lines.push(title_entry);
    }

    let mut body = out_lines.join("\n");
    body.push('\n');
    fs::write(&fork_path, body)?;
    Ok(ForkSessionResult {
        session_id: new_session_id,
    })
}

/// Crate-internal wrapper so other modules (e.g. `sessions_via_store`) can
/// reuse the same validator without duplicating the regex/format logic.
/// Not part of the public API.
///
/// True if `s` is a canonical 8-4-4-4-12 hex UUID string. Shared across
/// modules that need a stateless pre-flight check.
pub(crate) fn is_valid_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
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
            .all(|p| p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn validate_uuid(s: &str) -> Result<(), Error> {
    if is_valid_uuid(s) {
        Ok(())
    } else {
        Err(Error::message_parse(format!("Invalid session_id: {s}")))
    }
}

/// Resolve the Claude projects directory. Honours `$CLAUDE_CONFIG_DIR`
/// (ignoring empty-string values), else falls back to
/// `~/.claude/projects`. Shared across `sessions`, `session_mutations`,
/// and `client`.
pub(crate) fn projects_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("CLAUDE_CONFIG_DIR") {
        let trimmed = custom.trim_end_matches('/');
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed).join("projects");
        }
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
        let project_dir =
            projects_dir().join(crate::session::scan::sanitize_path_public(&canonical));
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

/// Rewrite `uuid` / `parentUuid` / `parent_uuid` / `sessionId` /
/// `session_id` on one JSONL entry using a fully-populated remap
/// (pass-1 output). Returns `true` when the entry's old uuid matched
/// `boundary` — caller stops after emitting that line.
pub(crate) fn remap_entry_fields(
    value: &mut Value,
    uuid_remap: &HashMap<String, String>,
    new_session_id: &str,
    boundary: Option<&str>,
) -> bool {
    let Some(obj) = value.as_object_mut() else {
        return false;
    };
    let mut boundary_hit = false;
    if let Some(old) = obj
        .get("uuid")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
    {
        if let Some(mapped) = uuid_remap.get(&old) {
            obj.insert("uuid".into(), Value::String(mapped.clone()));
        }
        if boundary == Some(old.as_str()) {
            boundary_hit = true;
        }
    }
    for parent_key in ["parentUuid", "parent_uuid"] {
        if let Some(parent) = obj
            .get(parent_key)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
        {
            if let Some(mapped) = uuid_remap.get(&parent) {
                obj.insert(parent_key.into(), Value::String(mapped.clone()));
            }
        }
    }
    for key in ["sessionId", "session_id"] {
        if obj.contains_key(key) {
            obj.insert(key.into(), Value::String(new_session_id.into()));
        }
    }
    boundary_hit
}

/// Scan the source transcript for the last `customTitle` / `aiTitle`
/// entry, or fall back to the first user prompt's text content.
fn derive_fork_title(source: &Path) -> Option<String> {
    let Ok(file) = fs::File::open(source) else {
        return None;
    };
    let mut custom_title: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(v) = value.get("customTitle").and_then(Value::as_str) {
            custom_title = Some(v.to_string());
        }
        if let Some(v) = value.get("aiTitle").and_then(Value::as_str) {
            ai_title = Some(v.to_string());
        }
        if first_prompt.is_none()
            && value.get("type").and_then(Value::as_str) == Some("user")
            && value.get("parent_tool_use_id").is_none_or(Value::is_null)
        {
            if let Some(content) = value
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
            {
                first_prompt = Some(content.to_string());
            }
        }
    }
    custom_title.or(ai_title).or(first_prompt)
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
