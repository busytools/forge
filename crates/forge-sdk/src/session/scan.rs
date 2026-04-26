//! Offline session scanners — stateless filesystem helpers that read
//! transcripts from `~/.claude/projects/<project_key>/*.jsonl`.
//!
//! Ports the public entry points of Python SDK v0.1.64
//! `_internal/sessions.py`:
//!
//! - [`list_sessions`] — lists sessions, either for one project or all.
//! - [`get_session_info`] — reads metadata for one session by ID.
//! - [`get_session_messages`] — reads the full transcript for one session.
//!
//! Session metadata ([`list_sessions`], [`get_session_info`]) is extracted
//! via an internal head + tail lite read — mirrors Python's
//! `_read_session_lite` / `_parse_session_info_from_lite` so a 100 MiB
//! transcript costs two 64 KiB reads rather than a full scan.
//!
//! Subagent helpers ([`list_subagents`], [`get_subagent_messages`]) read
//! `agent-<id>.jsonl` files under `<session_id>/subagents/` and recurse
//! into nested subdirectories (e.g. `workflows/<run_id>/`) to match
//! Python's layout.
//!
//! Not yet ported: the `SessionStore`-backed `*_from_store` variants
//! from `_internal/sessions.py`. Follow-up work.

use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;
use unicode_normalization::UnicodeNormalization;

use crate::public_types::{SDKSessionInfo, SessionMessage, SessionMessageKind};
use crate::session::mutations::is_valid_uuid;
use crate::session::mutations::projects_dir;

const MAX_SANITIZED_LENGTH: usize = 200;

/// Size of the head / tail byte buffer for lite metadata reads.
/// Python SDK constant (`_internal/sessions.py:29`) — match exactly so
/// the two implementations slice transcripts at the same boundary.
const LITE_READ_BUF_SIZE: u64 = 65_536;

/// Crate-internal re-export of the path sanitiser — other modules need
/// it to derive the same on-disk project-key layout the CLI uses. Not
/// part of the public API; downstream consumers should call
/// [`project_key_for_directory`] instead.
#[must_use]
pub(crate) fn sanitize_path_public(name: &str) -> String {
    sanitize_path(name)
}

/// Map a directory path to the CLI's on-disk project key. Canonicalises
/// the path first and then applies the CLI's JS-style sanitisation hash.
/// Mirrors Python SDK's `project_key_for_directory`
/// (`_internal/session_store.py`). `None` defaults to `"."` (the
/// process's current working directory), matching Python's
/// `directory: str | Path | None = None` signature.
#[must_use]
pub fn project_key_for_directory(path: Option<&str>) -> String {
    sanitize_path(&canonicalize_path(path.unwrap_or(".")))
}

/// Resolve a directory to its realpath and apply NFC normalisation.
/// Mirrors Python's `_canonicalize_path` (`_internal/sessions.py:147-153`)
/// — falls back to the input (NFC-normalised) when the path can't be
/// canonicalised (most commonly because it doesn't exist). NFC is
/// essential on filesystems that don't auto-normalise (Linux ext4,
/// Windows NTFS) so decomposed inputs still hash to the CLI's on-disk
/// project-key layout.
fn canonicalize_path(path: &str) -> String {
    let resolved = match fs::canonicalize(path) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(_) => path.to_string(),
    };
    resolved.nfc().collect()
}

/// List subagent IDs for a session. Subagent transcripts live at
/// `<projects_dir>/<project_key>/<session_id>/subagents/agent-<agent_id>.jsonl`
/// and may be nested in further subdirectories (e.g.
/// `subagents/workflows/<run_id>/agent-<agent_id>.jsonl`) — this
/// function recursively walks the tree. Ported from Python SDK v0.1.64
/// `list_subagents` (`_internal/sessions.py:1273-1316`).
///
/// Returns an empty Vec when `session_id` is not a valid UUID, the
/// session has no subagents directory, or no `agent-*.jsonl` files are
/// present.
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn list_subagents(session_id: &str, directory: Option<String>) -> Vec<String> {
    if !is_valid_uuid(session_id) {
        return Vec::new();
    }
    let Some(subagents_dir) = resolve_subagents_dir(session_id, directory.as_deref()) else {
        return Vec::new();
    };
    collect_agent_files(&subagents_dir)
        .into_iter()
        .map(|(agent_id, _)| agent_id)
        .collect()
}

/// Read a subagent's transcript in chronological order. Mirrors Python
/// SDK's `get_subagent_messages` (`_internal/sessions.py:1318-1383`).
///
/// `agent_id` is the id returned by [`list_subagents`] (the part between
/// `agent-` and `.jsonl` in the on-disk filename). `limit` caps the
/// number of messages returned; `offset` skips the first N.
///
/// Returns an empty Vec when `session_id` is not a valid UUID,
/// `agent_id` is empty, the transcript can't be found, or the file
/// contains no user/assistant entries.
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn get_subagent_messages(
    session_id: &str,
    agent_id: &str,
    directory: Option<String>,
    limit: Option<usize>,
    offset: usize,
) -> Vec<SessionMessage> {
    if !is_valid_uuid(session_id) || agent_id.is_empty() {
        return Vec::new();
    }
    let Some(subagents_dir) = resolve_subagents_dir(session_id, directory.as_deref()) else {
        return Vec::new();
    };
    // Walk the tree — the file may live directly under subagents/ or in
    // a nested subdirectory (Python `workflows/<runId>/` pattern).
    let Some((_, path)) = collect_agent_files(&subagents_dir)
        .into_iter()
        .find(|(found, _)| found == agent_id)
    else {
        return Vec::new();
    };
    let Ok(file) = fs::File::open(&path) else {
        return Vec::new();
    };
    let all = parse_session_messages(file);
    apply_limit_offset(all, limit, offset)
}

/// Recursively walk `base_dir` and collect `(agent_id, file_path)` for
/// every file named `agent-<agent_id>.jsonl`. Returned entries are
/// sorted by filename within each directory (matches Python's
/// `sorted(current_dir.iterdir(), key=lambda p: p.name)`).
fn collect_agent_files(base_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    walk_agent_files(base_dir, &mut out);
    out
}

fn walk_agent_files(dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(iter) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = iter.flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let Ok(ty) = entry.file_type() else { continue };
        if ty.is_file() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && let Some(stripped) = name.strip_prefix("agent-")
                && let Some(id) = stripped.strip_suffix(".jsonl")
            {
                out.push((id.to_string(), path));
            }
        } else if ty.is_dir() {
            walk_agent_files(&path, out);
        }
    }
}

fn apply_limit_offset(
    messages: Vec<SessionMessage>,
    limit: Option<usize>,
    offset: usize,
) -> Vec<SessionMessage> {
    let end = limit.map_or(messages.len(), |l| offset.saturating_add(l));
    messages
        .into_iter()
        .skip(offset)
        .take(end.saturating_sub(offset))
        .collect()
}

fn resolve_subagents_dir(session_id: &str, directory: Option<&str>) -> Option<PathBuf> {
    let project_dir = if let Some(dir) = directory {
        project_dir_for(dir)
    } else {
        let iter = fs::read_dir(projects_dir()).ok()?;
        iter.flatten()
            .map(|e| e.path())
            .find(|p| p.join(format!("{session_id}.jsonl")).is_file())?
    };
    Some(project_dir.join(session_id).join("subagents"))
}

fn parse_session_messages<R: std::io::Read>(reader: R) -> Vec<SessionMessage> {
    let mut out = Vec::new();
    for (idx, line_res) in BufReader::new(reader).lines().enumerate() {
        let line = match line_res {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(
                    line_no = idx,
                    error = %e,
                    "session scan: read failed; truncating message list"
                );
                break;
            }
        };
        if line.is_empty() {
            continue;
        }
        let value = match serde_json::from_str::<Value>(&line) {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(
                    line_no = idx,
                    error = %e,
                    "session scan: skipping unparseable line"
                );
                continue;
            }
        };
        let kind = match value.get("type").and_then(Value::as_str) {
            Some("user") => SessionMessageKind::User,
            Some("assistant") => SessionMessageKind::Assistant,
            _ => continue,
        };
        if value
            .get("parent_tool_use_id")
            .is_some_and(|v| !v.is_null())
        {
            continue;
        }
        let uuid = value
            .get("uuid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let sess = value
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let message = value.get("message").cloned().unwrap_or(Value::Null);
        out.push(SessionMessage {
            kind,
            uuid,
            session_id: sess,
            message,
            parent_tool_use_id: None,
        });
    }
    out
}

/// Sanitise a path the same way the `claude` CLI does — non-alphanumerics
/// become hyphens, and overlong paths are truncated with a base-36 hash
/// suffix (matching JS's `String.prototype.hashCode` trick). Ported from
/// Python `_sanitize_path` (`sessions.py:100-110`).
fn sanitize_path(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if sanitized.len() <= MAX_SANITIZED_LENGTH {
        return sanitized;
    }
    let hash = simple_hash(name);
    let truncated: String = sanitized.chars().take(MAX_SANITIZED_LENGTH).collect();
    format!("{truncated}-{hash}")
}

/// 32-bit integer hash to base-36, matching the CLI's directory naming.
fn simple_hash(s: &str) -> String {
    let mut h: i64 = 0;
    for ch in s.chars() {
        let c = ch as i64;
        h = (h << 5).wrapping_sub(h).wrapping_add(c);
        // Emulate JS `hash |= 0` (coerce to 32-bit signed int)
        h &= 0xFFFF_FFFF;
        if h >= 0x8000_0000 {
            h -= 0x1_0000_0000;
        }
    }
    let mut n = h.unsigned_abs();
    if n == 0 {
        return "0".into();
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    while n > 0 {
        out.push(digits[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

fn project_dir_for(project_path: &str) -> PathBuf {
    projects_dir().join(sanitize_path(&canonicalize_path(project_path)))
}

/// List sessions. When `directory` is `Some`, scans that project dir;
/// when `None`, scans every project directory. Results are sorted by
/// `last_modified` descending and pagination applies at the end.
///
/// # Panics
///
/// Never — filesystem errors fall through and produce an empty Vec.
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn list_sessions(
    directory: Option<String>,
    limit: Option<usize>,
    offset: usize,
) -> Vec<SDKSessionInfo> {
    let search_dirs: Vec<PathBuf> = if let Some(dir) = directory {
        vec![project_dir_for(&dir)]
    } else {
        fs::read_dir(projects_dir())
            .map(|iter| {
                iter.flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default()
    };

    let mut entries: Vec<SDKSessionInfo> = Vec::new();
    for project_dir in search_dirs {
        let Ok(iter) = fs::read_dir(&project_dir) else {
            continue;
        };
        for entry in iter.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(info) = read_session_info(&path) {
                entries.push(info);
            }
        }
    }

    entries.sort_by_key(|e| std::cmp::Reverse(e.last_modified));
    let end = limit.map_or(entries.len(), |l| offset.saturating_add(l));
    entries
        .into_iter()
        .skip(offset)
        .take(end.saturating_sub(offset))
        .collect()
}

/// Read metadata for one session. When `directory` is `None`, every
/// project directory is searched for a matching `<session_id>.jsonl`.
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn get_session_info(session_id: &str, directory: Option<String>) -> Option<SDKSessionInfo> {
    if !is_valid_uuid(session_id) {
        return None;
    }
    let file_name = format!("{session_id}.jsonl");
    if let Some(dir) = directory {
        return read_session_info(&project_dir_for(&dir).join(&file_name));
    }
    let projects = projects_dir();
    let iter = fs::read_dir(projects).ok()?;
    for entry in iter.flatten() {
        let candidate = entry.path().join(&file_name);
        if candidate.is_file() {
            return read_session_info(&candidate);
        }
    }
    None
}

/// Read the full transcript for one session. Returns an empty Vec when
/// the session file can't be found or parsed.
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn get_session_messages(session_id: &str, directory: Option<String>) -> Vec<SessionMessage> {
    if !is_valid_uuid(session_id) {
        return Vec::new();
    }
    let file_name = format!("{session_id}.jsonl");
    let candidate = if let Some(dir) = directory {
        Some(project_dir_for(&dir).join(&file_name))
    } else {
        fs::read_dir(projects_dir()).ok().and_then(|iter| {
            iter.flatten()
                .map(|e| e.path().join(&file_name))
                .find(|p| p.is_file())
        })
    };
    let Some(path) = candidate else {
        return Vec::new();
    };
    let Ok(file) = fs::File::open(&path) else {
        return Vec::new();
    };
    parse_session_messages(file)
}

// ---------------------------------------------------------------------------
// Lite read — head + tail metadata extraction without full-file scan.
// Ported from Python SDK v0.1.64 `_internal/sessions.py:347-441`.
// ---------------------------------------------------------------------------

/// Head / tail snapshot of a session file — enough to recover all
/// [`SDKSessionInfo`] fields without a full scan. Python equivalent:
/// `_LiteSessionFile` (`sessions.py:336-347`).
struct LiteSessionFile {
    mtime: u64,
    size: u64,
    head: String,
    tail: String,
}

/// Open a session file, stat it, read at most [`LITE_READ_BUF_SIZE`]
/// bytes from the head and the same from the tail. For files smaller
/// than the buffer, `tail == head` (single read). Returns `None` on
/// any I/O error or for empty files.
fn read_session_lite(path: &Path) -> Option<LiteSessionFile> {
    let mut file = fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    let size = meta.len();
    if size == 0 {
        return None;
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));

    let head_len = usize::try_from(LITE_READ_BUF_SIZE.min(size)).unwrap_or(usize::MAX);
    let mut head_bytes = vec![0u8; head_len];
    let read = file.read(&mut head_bytes).ok()?;
    head_bytes.truncate(read);
    if head_bytes.is_empty() {
        return None;
    }
    let head = String::from_utf8_lossy(&head_bytes).into_owned();

    let tail = if size <= LITE_READ_BUF_SIZE {
        head.clone()
    } else {
        let tail_offset = size - LITE_READ_BUF_SIZE;
        file.seek(SeekFrom::Start(tail_offset)).ok()?;
        let mut tail_bytes = vec![0u8; usize::try_from(LITE_READ_BUF_SIZE).unwrap_or(usize::MAX)];
        let read = file.read(&mut tail_bytes).ok()?;
        tail_bytes.truncate(read);
        String::from_utf8_lossy(&tail_bytes).into_owned()
    };

    Some(LiteSessionFile {
        mtime,
        size,
        head,
        tail,
    })
}

/// Find the first byte offset where `needle` begins in `haystack`.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Extract the first occurrence of a JSON string field (`"key":"value"`
/// or `"key": "value"`). Scans bytes directly to survive partial tail
/// reads; unescapes via `serde_json` only when the value contains a
/// backslash. Returns `None` when the field is absent or unterminated.
fn extract_json_string_field(text: &str, key: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let compact = format!("\"{key}\":\"");
    let spaced = format!("\"{key}\": \"");
    for pattern in [compact.as_bytes(), spaced.as_bytes()] {
        if let Some(idx) = find_bytes(bytes, pattern) {
            let value_start = idx + pattern.len();
            let mut i = value_start;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    let raw = std::str::from_utf8(&bytes[value_start..i]).ok()?;
                    return Some(unescape_json_string(raw));
                }
                i += 1;
            }
        }
    }
    None
}

/// Like [`extract_json_string_field`] but returns the LAST occurrence.
fn extract_last_json_string_field(text: &str, key: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let compact = format!("\"{key}\":\"");
    let spaced = format!("\"{key}\": \"");
    let mut last: Option<String> = None;
    for pattern in [compact.as_bytes(), spaced.as_bytes()] {
        let mut search_from = 0usize;
        while search_from < bytes.len() {
            let remaining = &bytes[search_from..];
            let Some(rel_idx) = find_bytes(remaining, pattern) else {
                break;
            };
            let idx = search_from + rel_idx;
            let value_start = idx + pattern.len();
            let mut i = value_start;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    if let Ok(raw) = std::str::from_utf8(&bytes[value_start..i]) {
                        last = Some(unescape_json_string(raw));
                    }
                    break;
                }
                i += 1;
            }
            search_from = i + 1;
        }
    }
    last
}

/// Unescape a JSON string value. No-op when there are no backslashes.
fn unescape_json_string(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_string();
    }
    let wrapped = format!("\"{raw}\"");
    serde_json::from_str::<String>(&wrapped).unwrap_or_else(|_| raw.to_string())
}

/// Extract the first meaningful user prompt from a JSONL head chunk.
/// Skips `tool_result`, `isMeta`, `isCompactSummary`, slash-command
/// messages (with command-name fallback), and the fixed-prefix skip
/// patterns Python's `_SKIP_FIRST_PROMPT_PATTERN` matches. Truncates to
/// 200 chars with an ellipsis. Ported from `sessions.py:255-330`.
#[allow(clippy::too_many_lines)]
fn extract_first_prompt_from_head(head: &str) -> Option<String> {
    let mut command_fallback: Option<String> = None;
    for line in head.split('\n') {
        if !line.contains("\"type\":\"user\"") && !line.contains("\"type\": \"user\"") {
            continue;
        }
        if line.contains("\"tool_result\"") {
            continue;
        }
        if line.contains("\"isMeta\":true") || line.contains("\"isMeta\": true") {
            continue;
        }
        if line.contains("\"isCompactSummary\":true") || line.contains("\"isCompactSummary\": true")
        {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let Some(message) = entry.get("message") else {
            continue;
        };
        let Some(content) = message.get("content") else {
            continue;
        };
        let texts: Vec<String> = if let Some(s) = content.as_str() {
            vec![s.to_string()]
        } else if let Some(arr) = content.as_array() {
            arr.iter()
                .filter_map(|b| {
                    (b.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| b.get("text").and_then(Value::as_str).map(str::to_string))
                        .flatten()
                })
                .collect()
        } else {
            Vec::new()
        };
        for raw in texts {
            let result = raw.replace('\n', " ");
            let result = result.trim();
            if result.is_empty() {
                continue;
            }
            if let Some(cmd) = extract_command_name(result) {
                if command_fallback.is_none() {
                    command_fallback = Some(cmd);
                }
                continue;
            }
            if should_skip_first_prompt(result) {
                continue;
            }
            let truncated = if result.chars().count() > 200 {
                let mut buf: String = result.chars().take(200).collect();
                while buf.ends_with(char::is_whitespace) {
                    buf.pop();
                }
                buf.push('\u{2026}');
                buf
            } else {
                result.to_string()
            };
            return Some(truncated);
        }
    }
    command_fallback
}

/// Extract `<command-name>CMD</command-name>` when present.
pub(crate) fn extract_command_name(s: &str) -> Option<String> {
    const OPEN: &str = "<command-name>";
    const CLOSE: &str = "</command-name>";
    let open = s.find(OPEN)?;
    let after = &s[open + OPEN.len()..];
    let close = after.find(CLOSE)?;
    Some(after[..close].to_string())
}

/// Fixed-prefix counterpart to Python's `_SKIP_FIRST_PROMPT_PATTERN`.
pub(crate) fn should_skip_first_prompt(s: &str) -> bool {
    const PREFIXES: [&str; 4] = [
        "<local-command-stdout>",
        "<session-start-hook>",
        "<tick>",
        "<goal>",
    ];
    if PREFIXES.iter().any(|p| s.starts_with(p)) {
        return true;
    }
    if s.starts_with("[Request interrupted by user") && s.contains(']') {
        return true;
    }
    let trimmed = s.trim();
    for (open, close) in [
        ("<ide_opened_file>", "</ide_opened_file>"),
        ("<ide_selection>", "</ide_selection>"),
    ] {
        if trimmed.starts_with(open) && trimmed.ends_with(close) {
            return true;
        }
    }
    false
}

fn read_session_info(path: &Path) -> Option<SDKSessionInfo> {
    let session_id = path.file_stem().and_then(|s| s.to_str())?.to_string();
    let lite = read_session_lite(path)?;
    parse_session_info_from_lite(&session_id, &lite, None)
}

/// Ported from Python `_parse_session_info_from_lite`
/// (`sessions.py:418-502`). Skips sidechain transcripts and metadata-only
/// sessions (no summary after all fallbacks).
fn parse_session_info_from_lite(
    session_id: &str,
    lite: &LiteSessionFile,
    project_path: Option<&str>,
) -> Option<SDKSessionInfo> {
    let head = lite.head.as_str();
    let tail = lite.tail.as_str();

    let first_line = head.find('\n').map_or(head, |idx| &head[..idx]);
    if first_line.contains("\"isSidechain\":true") || first_line.contains("\"isSidechain\": true") {
        return None;
    }

    let custom_title = extract_last_json_string_field(tail, "customTitle")
        .or_else(|| extract_last_json_string_field(head, "customTitle"))
        .or_else(|| extract_last_json_string_field(tail, "aiTitle"))
        .or_else(|| extract_last_json_string_field(head, "aiTitle"));
    let first_prompt = extract_first_prompt_from_head(head);
    let summary = custom_title
        .clone()
        .or_else(|| extract_last_json_string_field(tail, "lastPrompt"))
        .or_else(|| extract_last_json_string_field(tail, "summary"))
        .or_else(|| first_prompt.clone())?;

    let git_branch = extract_last_json_string_field(tail, "gitBranch")
        .or_else(|| extract_json_string_field(head, "gitBranch"));
    let cwd = extract_json_string_field(head, "cwd").or_else(|| project_path.map(str::to_string));
    // Scope tag extraction to `{"type":"tag"}` lines — a bare scan for
    // `"tag"` would match tool_use inputs (git tag, Docker tags, etc.).
    let tag = tail
        .lines()
        .rev()
        .find(|l| l.starts_with("{\"type\":\"tag\""))
        .and_then(|l| extract_last_json_string_field(l, "tag"))
        .filter(|v| !v.is_empty());
    let created_at = extract_json_string_field(first_line, "timestamp")
        .and_then(|ts| chrono_like_parse_ms(&ts).ok());

    Some(SDKSessionInfo {
        session_id: session_id.to_string(),
        summary,
        last_modified: lite.mtime,
        file_size: Some(lite.size),
        custom_title,
        first_prompt,
        git_branch,
        cwd,
        tag,
        created_at,
    })
}

/// Best-effort ISO-8601 → milliseconds converter. No chrono dep; handles
/// the specific `YYYY-MM-DDTHH:MM:SS(.sss)?Z` shape the CLI emits.
pub(crate) fn chrono_like_parse_ms(ts: &str) -> Result<u64, ()> {
    // Example: "2026-04-22T04:15:27.123Z"
    let bytes = ts.as_bytes();
    if bytes.len() < 20 || !ts.ends_with('Z') {
        return Err(());
    }
    let year: i32 = ts.get(0..4).and_then(|s| s.parse().ok()).ok_or(())?;
    let month: u32 = ts.get(5..7).and_then(|s| s.parse().ok()).ok_or(())?;
    let day: u32 = ts.get(8..10).and_then(|s| s.parse().ok()).ok_or(())?;
    let hour: u32 = ts.get(11..13).and_then(|s| s.parse().ok()).ok_or(())?;
    let minute: u32 = ts.get(14..16).and_then(|s| s.parse().ok()).ok_or(())?;
    let second: u32 = ts.get(17..19).and_then(|s| s.parse().ok()).ok_or(())?;
    // Sub-second fragment — normalise to 3-digit millis. "x.5Z" → 500,
    // "x.123456Z" → 123 (not 123456, which was ~2 minutes wrong).
    let mut millis: u32 = 0;
    if bytes.get(19) == Some(&b'.') {
        let ms_end = ts.find('Z').unwrap_or(bytes.len());
        if let Some(frag) = ts.get(20..ms_end) {
            let mut buf = String::with_capacity(3);
            for ch in frag.chars().take(3) {
                buf.push(ch);
            }
            while buf.len() < 3 {
                buf.push('0');
            }
            millis = buf.parse::<u32>().unwrap_or(0).min(999);
        }
    }

    // Simple epoch conversion valid for 1970-02-01 through ~2300. Cap
    // the upper year so a malformed "99999-..." timestamp can't spin
    // the leap-year loop ~98 000 iterations per entry.
    if !(1970..=2300).contains(&year) {
        return Err(());
    }
    let mut days: u64 = 0;
    for y in 1970..year {
        let leap = is_leap(y);
        days += if leap { 366 } else { 365 };
    }
    let ml = month_lengths(year);
    for (i, len) in ml.iter().enumerate() {
        let idx = u32::try_from(i).unwrap_or(u32::MAX);
        if idx + 1 >= month {
            break;
        }
        days += u64::from(*len);
    }
    days += u64::from(day.saturating_sub(1));
    let total_seconds: u64 =
        days * 86_400 + u64::from(hour) * 3_600 + u64::from(minute) * 60 + u64::from(second);
    Ok(total_seconds * 1_000 + u64::from(millis))
}

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn month_lengths(year: i32) -> [u32; 12] {
    let feb = if is_leap(year) { 29 } else { 28 };
    [31, feb, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn sanitize_ascii_only_passthrough() {
        assert_eq!(sanitize_path("alphanum123"), "alphanum123");
    }

    #[test]
    fn sanitize_replaces_non_alphanum_with_hyphens() {
        assert_eq!(
            sanitize_path("/Users/vedhavyas/projects/forge"),
            "-Users-vedhavyas-projects-forge"
        );
    }

    #[test]
    fn simple_hash_matches_known_value() {
        // Python reference: _simple_hash("foo") → "26di" (computed from the
        // same 32-bit JS-style hash algorithm).
        assert_eq!(simple_hash("foo"), "26di");
    }

    #[test]
    fn long_path_gets_hash_suffix() {
        let long = "a".repeat(300);
        let got = sanitize_path(&long);
        assert_eq!(
            got.len(),
            MAX_SANITIZED_LENGTH + 1 + simple_hash(&long).len()
        );
    }

    #[test]
    fn uuid_validator_accepts_canonical() {
        assert!(is_valid_uuid("550e8400-e29b-41d4-a716-446655440000"));
    }

    #[test]
    fn uuid_validator_rejects_garbage() {
        assert!(!is_valid_uuid("not-a-uuid"));
        assert!(!is_valid_uuid("550e8400e29b41d4a716446655440000"));
        assert!(!is_valid_uuid(""));
    }

    #[test]
    fn iso_parser_handles_millis() {
        let ms = chrono_like_parse_ms("2026-04-22T00:00:00.500Z").unwrap();
        // 2026-04-22 is 20200 days after 1970-01-01.
        // Verify via cross-check: seconds = 20200 * 86400.
        // We don't hard-code; just check the ms portion adds correctly.
        assert_eq!(ms % 1000, 500);
    }

    #[test]
    fn extract_json_string_field_finds_compact_form() {
        let t = r#"noise {"type":"user","message":{"content":"hi"}} noise"#;
        assert_eq!(
            extract_json_string_field(t, "content"),
            Some("hi".to_string())
        );
    }

    #[test]
    fn extract_json_string_field_finds_spaced_form() {
        let t = r#"{"gitBranch": "main"}"#;
        assert_eq!(
            extract_json_string_field(t, "gitBranch"),
            Some("main".to_string())
        );
    }

    #[test]
    fn extract_json_string_field_handles_escaped_quotes() {
        let t = r#"{"customTitle":"he said \"hi\""}"#;
        assert_eq!(
            extract_json_string_field(t, "customTitle"),
            Some(r#"he said "hi""#.to_string())
        );
    }

    #[test]
    fn extract_last_json_string_field_picks_last() {
        let t = r#"{"tag":"old"} {"tag":"new"}"#;
        assert_eq!(
            extract_last_json_string_field(t, "tag"),
            Some("new".to_string())
        );
    }

    #[test]
    fn first_prompt_skips_local_command_stdout() {
        let head = r#"{"type":"user","message":{"content":"<local-command-stdout>out</local-command-stdout>"}}
{"type":"user","message":{"content":"actual prompt"}}"#;
        assert_eq!(
            extract_first_prompt_from_head(head),
            Some("actual prompt".to_string())
        );
    }

    #[test]
    fn first_prompt_falls_back_to_command_name() {
        let head = r#"{"type":"user","message":{"content":"<command-name>foo</command-name>"}}"#;
        assert_eq!(
            extract_first_prompt_from_head(head),
            Some("foo".to_string())
        );
    }

    #[test]
    fn first_prompt_skips_tool_result_line() {
        let head = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"x"}]}}
{"type":"user","message":{"content":"real prompt"}}"#;
        assert_eq!(
            extract_first_prompt_from_head(head),
            Some("real prompt".to_string())
        );
    }

    #[test]
    fn parse_session_info_skips_sidechain() {
        let head = "{\"isSidechain\":true,\"type\":\"user\"}\n".to_string();
        let lite = LiteSessionFile {
            mtime: 0,
            size: 1,
            head: head.clone(),
            tail: head,
        };
        assert!(parse_session_info_from_lite("abc", &lite, None).is_none());
    }

    #[test]
    fn parse_session_info_skips_metadata_only() {
        let content = "{\"type\":\"tag\",\"tag\":\"meta\"}\n".to_string();
        let lite = LiteSessionFile {
            mtime: 10,
            size: content.len() as u64,
            head: content.clone(),
            tail: content,
        };
        // No custom_title, no aiTitle, no lastPrompt, no summary,
        // no first_prompt → skipped.
        assert!(parse_session_info_from_lite("abc", &lite, None).is_none());
    }

    #[test]
    fn parse_session_info_extracts_prompt_and_tag() {
        let content = r#"{"type":"user","timestamp":"2026-04-22T00:00:00.000Z","gitBranch":"main","cwd":"/p","message":{"content":"hello"}}
{"type":"tag","tag":"mytag"}
"#
        .to_string();
        let lite = LiteSessionFile {
            mtime: 99,
            size: content.len() as u64,
            head: content.clone(),
            tail: content,
        };
        let info = parse_session_info_from_lite("abc", &lite, None).expect("some");
        assert_eq!(info.first_prompt.as_deref(), Some("hello"));
        assert_eq!(info.summary, "hello");
        assert_eq!(info.tag.as_deref(), Some("mytag"));
        assert_eq!(info.git_branch.as_deref(), Some("main"));
        assert_eq!(info.cwd.as_deref(), Some("/p"));
        assert!(info.created_at.is_some());
    }

    #[test]
    fn parse_session_info_ignores_tag_on_tool_use_lines() {
        // A git-tag tool_use shouldn't be picked up as a session tag —
        // the `"tag"` string appears but the line isn't `{"type":"tag"`.
        let content = r#"{"type":"user","message":{"content":"hi"}}
{"type":"assistant","message":{"content":[{"type":"tool_use","input":{"command":"git tag","tag":"v1.0"}}]}}
"#
        .to_string();
        let lite = LiteSessionFile {
            mtime: 0,
            size: content.len() as u64,
            head: content.clone(),
            tail: content,
        };
        let info = parse_session_info_from_lite("abc", &lite, None).expect("some");
        assert_eq!(info.tag, None);
    }

    #[test]
    fn parse_session_info_prefers_custom_title_over_last_prompt() {
        let content = r#"{"type":"user","message":{"content":"initial"}}
{"customTitle":"Curated","lastPrompt":"last"}
"#
        .to_string();
        let lite = LiteSessionFile {
            mtime: 0,
            size: content.len() as u64,
            head: content.clone(),
            tail: content,
        };
        let info = parse_session_info_from_lite("abc", &lite, None).expect("some");
        assert_eq!(info.summary, "Curated");
        assert_eq!(info.custom_title.as_deref(), Some("Curated"));
    }

    // ---------------------------------------------------------------------
    // Subagent-listing helpers — the recursive walk + filename filter
    // ported from Python `_collect_agent_files`
    // (`_internal/sessions.py:1200-1228`).
    // ---------------------------------------------------------------------

    fn write_tmp_file(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    #[test]
    fn collect_agent_files_picks_agent_prefixed_jsonl_only() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        write_tmp_file(&base.join("agent-aaa.jsonl"), "{}\n");
        write_tmp_file(&base.join("random.jsonl"), "{}\n"); // decoy
        write_tmp_file(&base.join("agent-bbb.txt"), "{}\n"); // wrong ext

        let collected = collect_agent_files(base);
        let ids: Vec<&str> = collected.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains(&"aaa"), "agent-aaa.jsonl must be collected");
        assert!(
            !ids.contains(&"bbb"),
            "agent-bbb.txt must be ignored (wrong extension)"
        );
        assert!(
            !ids.contains(&"random"),
            "random.jsonl must be ignored (missing `agent-` prefix)"
        );
    }

    #[test]
    fn collect_agent_files_recurses_into_nested_subdirs() {
        // Python writes subagents at `workflows/<run_id>/agent-<id>.jsonl`
        // (`_internal/sessions.py:1282`). Walk must find them.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        write_tmp_file(
            &base
                .join("workflows")
                .join("run1")
                .join("agent-nested.jsonl"),
            "{}\n",
        );
        let collected = collect_agent_files(base);
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].0, "nested");
        assert!(collected[0].1.ends_with("agent-nested.jsonl"));
    }

    #[test]
    fn collect_agent_files_returns_empty_for_missing_dir() {
        let collected = collect_agent_files(Path::new("/nonexistent/path/xyz"));
        assert!(collected.is_empty());
    }

    #[test]
    fn apply_limit_offset_slices() {
        let make = |n: usize| SessionMessage {
            kind: SessionMessageKind::User,
            uuid: format!("u-{n}"),
            session_id: "s".into(),
            message: Value::Null,
            parent_tool_use_id: None,
        };
        let msgs = vec![make(0), make(1), make(2), make(3)];
        assert_eq!(apply_limit_offset(msgs.clone(), None, 0).len(), 4);
        assert_eq!(apply_limit_offset(msgs.clone(), Some(2), 0).len(), 2);
        assert_eq!(apply_limit_offset(msgs.clone(), Some(2), 1).len(), 2);
        assert_eq!(apply_limit_offset(msgs.clone(), Some(10), 3).len(), 1);
        assert_eq!(apply_limit_offset(msgs, Some(0), 0).len(), 0);
    }
}
