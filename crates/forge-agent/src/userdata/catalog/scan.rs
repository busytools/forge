//! Offline session scanners - stateless filesystem helpers that read
//! transcripts from `<config_dir>/projects/<project_key>/*.jsonl`.
//!
//! - [`list_sessions`] - lists sessions, either for one project or all.
//! - [`get_session_info`] - reads metadata for one session by ID.
//! - [`get_session_messages`] - reads the full transcript for one session.
//!
//! Session metadata ([`list_sessions`], [`get_session_info`]) is
//! extracted via an internal head + tail lite read so a 100 MiB
//! transcript costs two 64 KiB reads rather than a full scan.
//!
//! Subagent helpers ([`list_subagents`], [`get_subagent_messages`])
//! read `agent-<id>.jsonl` files under `<session_id>/subagents/` and
//! recurse into nested subdirectories (e.g. `workflows/<run_id>/`)
//! to match the CLI's on-disk layout.

use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use forge_primitives::{
    FORGE_WORKER_TAG_PREFIX, SDKSessionInfo, SessionMessage, SessionMessageKind,
};
use forge_sdk::projects_dir_for;

/// True if `s` is a canonical 8-4-4-4-12 hyphenated UUID. The length
/// guard rejects the hyphenless / braced / URN forms that
/// `Uuid::try_parse` otherwise accepts - session ids on disk are always
/// the hyphenated form the CLI emits.
pub(crate) fn is_valid_uuid(s: &str) -> bool {
    s.len() == 36 && Uuid::try_parse(s).is_ok()
}

const MAX_SANITIZED_LENGTH: usize = 200;

/// Open `dir` for directory iteration. NotFound is the expected case
/// for the catalog's projects/ tree on a fresh forge install and is
/// silent; real I/O failures (perm denied, broken FS) log at warn
/// so the user gets a triage signal rather than silently empty
/// catalog reads.
fn try_read_dir(dir: &Path) -> Option<fs::ReadDir> {
    match fs::read_dir(dir) {
        Ok(iter) => Some(iter),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            tracing::warn!(
                target: "forge_agent::userdata::catalog",
                path = %dir.display(),
                error = %e,
                "failed to read catalog directory"
            );
            None
        }
    }
}

/// Size of the head / tail byte buffer for lite metadata reads.
/// The CLI constant - match exactly so
/// the two implementations slice transcripts at the same boundary.
const LITE_READ_BUF_SIZE: u64 = 65_536;

/// Crate-internal re-export of the path sanitiser - other modules need
/// it to derive the same on-disk project-key layout the CLI uses. Not
/// part of the public API; downstream consumers should call
/// [`project_key_for_directory`] instead.
pub(crate) fn sanitize_path_public(name: &str) -> String {
    sanitize_path(name)
}

/// Map a directory path to the CLI's on-disk project key. Canonicalises
/// the path first and then applies the CLI's JS-style sanitisation
/// hash. `None` defaults to `"."` (the process's current working
/// directory).
pub fn project_key_for_directory(path: Option<&str>) -> String {
    sanitize_path(&canonicalize_path(path.unwrap_or(".")))
}

/// Resolve a directory to its realpath and apply NFC normalisation.
/// Wraps the CLI's `_canonicalize_path` (falls back to the input,
/// NFC-normalised, when the path can't be canonicalised: most
/// commonly because it doesn't exist). NFC is essential on
/// filesystems that don't auto-normalise (Linux ext4, Windows NTFS)
/// so decomposed inputs still hash to the CLI's on-disk
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
/// `subagents/workflows/<run_id>/agent-<agent_id>.jsonl`) - this
/// function recursively walks the tree.
///
/// Returns an empty Vec when `session_id` is not a valid UUID, the
/// session has no subagents directory, or no `agent-*.jsonl` files are
/// present.
pub fn list_subagents(config_dir: &Path, session_id: &str, directory: Option<&str>) -> Vec<String> {
    if !is_valid_uuid(session_id) {
        return Vec::new();
    }
    let Some(subagents_dir) = resolve_subagents_dir(config_dir, session_id, directory) else {
        return Vec::new();
    };
    collect_agent_files(&subagents_dir).into_iter().map(|(agent_id, _)| agent_id).collect()
}

/// Read a subagent's transcript in chronological order.
///
/// `agent_id` is the id returned by [`list_subagents`] (the part between
/// `agent-` and `.jsonl` in the on-disk filename). `limit` caps the
/// number of messages returned; `offset` skips the first N.
///
/// Returns an empty Vec when `session_id` is not a valid UUID,
/// `agent_id` is empty, the transcript can't be found, or the file
/// contains no user/assistant entries.
pub fn get_subagent_messages(
    config_dir: &Path,
    session_id: &str,
    agent_id: &str,
    directory: Option<&str>,
    limit: Option<usize>,
    offset: usize,
) -> Vec<SessionMessage> {
    if !is_valid_uuid(session_id) || agent_id.is_empty() {
        return Vec::new();
    }
    let Some(subagents_dir) = resolve_subagents_dir(config_dir, session_id, directory) else {
        return Vec::new();
    };
    // Walk the tree - the file may live directly under subagents/ or
    // in a nested subdirectory (e.g. `workflows/<run_id>/`).
    let Some((_, path)) =
        collect_agent_files(&subagents_dir).into_iter().find(|(found, _)| found == agent_id)
    else {
        return Vec::new();
    };
    let Ok(file) = fs::File::open(&path) else {
        return Vec::new();
    };
    let all = parse_session_messages(file);
    apply_limit_offset(all, limit, offset)
}

/// Recursively walk `base_dir` and collect `(agent_id, file_path)`
/// for every file named `agent-<agent_id>.jsonl`. Returned entries
/// are sorted by filename within each directory (matches the CLI's
/// on-disk traversal order so [`list_subagents`] is reproducible).
fn collect_agent_files(base_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    walk_agent_files(base_dir, &mut out);
    out
}

fn walk_agent_files(dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Some(iter) = try_read_dir(dir) else {
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
    messages.into_iter().skip(offset).take(end.saturating_sub(offset)).collect()
}

fn resolve_subagents_dir(
    config_dir: &Path,
    session_id: &str,
    directory: Option<&str>,
) -> Option<PathBuf> {
    let project_dir = if let Some(dir) = directory {
        project_dir_for(config_dir, dir)
    } else {
        let iter = try_read_dir(&projects_dir_for(config_dir))?;
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
        let row_type = value.get("type").and_then(Value::as_str);
        let (kind, message) = match row_type {
            Some("user") => (SessionMessageKind::User, value.get("message").cloned()),
            Some("assistant") => (SessionMessageKind::Assistant, value.get("message").cloned()),
            // Attachment rows hold claude's persisted record of mid-turn
            // queued inputs - `{"type":"attachment", "attachment":{"type":
            // "queued_command", "prompt":"...", "commandMode":"prompt"}}`.
            // On replay we hoist them into a synthetic user envelope
            // whose single content block is the `queued_command`, so the
            // downstream walker reconstructs the user bubble that was
            // never on the wire as a regular user message.
            Some("attachment") => match value.get("attachment") {
                Some(att) if att.get("type").and_then(Value::as_str) == Some("queued_command") => {
                    (SessionMessageKind::User, Some(synthesize_queued_command_message(att)))
                }
                _ => continue,
            },
            _ => continue,
        };
        if value.get("parent_tool_use_id").is_some_and(|v| !v.is_null()) {
            continue;
        }
        let uuid = value.get("uuid").and_then(Value::as_str).unwrap_or_default().to_string();
        let sess = value.get("session_id").and_then(Value::as_str).unwrap_or_default().to_string();
        out.push(SessionMessage {
            kind,
            uuid,
            session_id: sess,
            message: message.unwrap_or(Value::Null),
            parent_tool_use_id: None,
        });
    }
    out
}

/// Build a `{"role":"user","content":[{queued_command}]}` envelope from
/// a JSONL attachment row's `attachment` field. Used during session
/// replay to surface mid-turn queued inputs that claude persisted as
/// `type:"attachment"` rows (which the scanner would otherwise skip).
fn synthesize_queued_command_message(attachment: &Value) -> Value {
    let mut block = serde_json::Map::new();
    block.insert("type".into(), Value::String("queued_command".into()));
    if let Some(prompt) = attachment.get("prompt") {
        block.insert("prompt".into(), prompt.clone());
    }
    if let Some(mode) = attachment.get("commandMode") {
        block.insert("commandMode".into(), mode.clone());
    }
    if let Some(src) = attachment.get("source_uuid") {
        block.insert("source_uuid".into(), src.clone());
    }
    serde_json::json!({
        "role": "user",
        "content": [Value::Object(block)],
    })
}

/// Sanitise a path the same way the `claude` CLI does -
/// non-alphanumerics become hyphens, and overlong paths are
/// truncated with a base-36 hash suffix (matching JS's
/// `String.prototype.hashCode` trick).
fn sanitize_path(name: &str) -> String {
    let sanitized: String =
        name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
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

fn project_dir_for(config_dir: &Path, project_path: &str) -> PathBuf {
    projects_dir_for(config_dir).join(sanitize_path(&canonicalize_path(project_path)))
}

/// Bounded-concurrency cap for [`list_sessions`] per-file lite reads.
///
/// Each transcript costs an open, a head read, a tail seek, and a tail
/// read, plus UTF-8 validation. On a project-rich install (~50 projects,
/// ~10 sessions each) the serial walk dominates connect time; capping
/// at 16 keeps fd pressure low while still saturating an SSD.
const LIST_SESSIONS_MAX_CONCURRENT: usize = 16;

/// List sessions. When `directory` is `Some`, scans that project dir;
/// when `None`, scans every project directory under `config_dir`'s
/// `projects/` tree. Per-file lite reads run on the tokio blocking
/// pool with bounded concurrency (capped at 16 concurrent reads);
/// results are sorted by `last_modified` descending and pagination
/// applies at the end.
///
/// # Panics
///
/// Never - filesystem errors fall through and produce an empty Vec.
/// True when `info` represents a worker session that should be
/// hidden from default `list_sessions` / session-picker / resolver
/// output. Callers opt in via `include_workers = true` to see them.
#[must_use]
pub fn should_exclude_worker_tag(info: &SDKSessionInfo) -> bool {
    info.tag.as_deref().is_some_and(|t| t.starts_with(FORGE_WORKER_TAG_PREFIX))
}

pub async fn list_sessions(
    config_dir: &Path,
    directory: Option<&str>,
    limit: Option<usize>,
    offset: usize,
    include_workers: bool,
) -> Vec<SDKSessionInfo> {
    let search_dirs: Vec<PathBuf> = if let Some(dir) = directory {
        vec![project_dir_for(config_dir, dir)]
    } else {
        try_read_dir(&projects_dir_for(config_dir))
            .map(|iter| {
                iter.flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default()
    };

    // Cheap directory walks first - collect every candidate path
    // synchronously. The expensive part is the per-file lite read,
    // which we hand off to spawn_blocking below.
    let mut candidates: Vec<PathBuf> = Vec::new();
    for project_dir in search_dirs {
        let Some(iter) = try_read_dir(&project_dir) else {
            continue;
        };
        for entry in iter.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                candidates.push(path);
            }
        }
    }

    let mut entries: Vec<SDKSessionInfo> = Vec::with_capacity(candidates.len());
    let mut paths = candidates.into_iter();
    let mut set: tokio::task::JoinSet<Option<SDKSessionInfo>> = tokio::task::JoinSet::new();
    for path in paths.by_ref().take(LIST_SESSIONS_MAX_CONCURRENT) {
        set.spawn_blocking(move || read_session_info(&path));
    }
    while let Some(res) = set.join_next().await {
        match res {
            Ok(Some(info)) => entries.push(info),
            Ok(None) => {}
            Err(err) => {
                tracing::debug!(
                    target: "forge_agent::catalog::scan",
                    error = %err,
                    "list_sessions: per-file read task failed",
                );
            }
        }
        if let Some(path) = paths.next() {
            set.spawn_blocking(move || read_session_info(&path));
        }
    }

    entries.sort_by_key(|e| std::cmp::Reverse(e.last_modified));
    let entries: Vec<SDKSessionInfo> = if include_workers {
        entries
    } else {
        entries.into_iter().filter(|info| !should_exclude_worker_tag(info)).collect()
    };
    let end = limit.map_or(entries.len(), |l| offset.saturating_add(l));
    entries.into_iter().skip(offset).take(end.saturating_sub(offset)).collect()
}

/// Read metadata for one session. When `directory` is `None`, every
/// project directory under `<config_dir>/projects/` is searched for
/// a matching `<session_id>.jsonl`.
pub fn get_session_info(
    config_dir: &Path,
    session_id: &str,
    directory: Option<&str>,
) -> Option<SDKSessionInfo> {
    if !is_valid_uuid(session_id) {
        return None;
    }
    let file_name = format!("{session_id}.jsonl");
    if let Some(dir) = directory {
        return read_session_info(&project_dir_for(config_dir, dir).join(&file_name));
    }
    let projects = projects_dir_for(config_dir);
    let iter = try_read_dir(&projects)?;
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
pub fn get_session_messages(
    config_dir: &Path,
    session_id: &str,
    directory: Option<&str>,
) -> Vec<SessionMessage> {
    if !is_valid_uuid(session_id) {
        return Vec::new();
    }
    let file_name = format!("{session_id}.jsonl");
    let candidate = if let Some(dir) = directory {
        Some(project_dir_for(config_dir, dir).join(&file_name))
    } else {
        try_read_dir(&projects_dir_for(config_dir)).and_then(|iter| {
            iter.flatten().map(|e| e.path().join(&file_name)).find(|p| p.is_file())
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
// Lite read - head + tail metadata extraction without full-file scan.
// ---------------------------------------------------------------------------

/// Head / tail snapshot of a session file - enough to recover all
/// [`SDKSessionInfo`] fields without a full scan. The `tag` field is
/// populated by a separate streaming line-scan because tag rows can
/// appear anywhere in the file (start-of-file at spawn, mid-file via
/// `/new` re-tag) and the head + tail windows miss both cases on
/// large transcripts.
struct LiteSessionFile {
    mtime: u64,
    size: u64,
    head: String,
    tail: String,
    tag: Option<String>,
}

/// Open a session file, stat it, read at most [`LITE_READ_BUF_SIZE`]
/// bytes from the head and the same from the tail. For files smaller
/// than the buffer, `tail == head` (single read). Returns `None` on
/// any I/O error or for empty files.
///
/// Each `.ok()?` early return logs at debug level naming the step
/// that failed - without these, the session picker silently drops
/// sessions whose files have permission errors / are mid-truncation
/// / have a bad fd, which presents to the user as missing sessions
/// with no triage signal.
fn read_session_lite(path: &Path) -> Option<LiteSessionFile> {
    let mut file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!(target: crate::logging::targets::CATALOG_SCAN, path = %path.display(), error = %e, step = "open", "lite-read failed");
            return None;
        }
    };
    let meta = match file.metadata() {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!(target: crate::logging::targets::CATALOG_SCAN, path = %path.display(), error = %e, step = "metadata", "lite-read failed");
            return None;
        }
    };
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
    let read = match file.read(&mut head_bytes) {
        Ok(n) => n,
        Err(e) => {
            tracing::debug!(target: crate::logging::targets::CATALOG_SCAN, path = %path.display(), error = %e, step = "read_head", "lite-read failed");
            return None;
        }
    };
    head_bytes.truncate(read);
    if head_bytes.is_empty() {
        return None;
    }
    let head = String::from_utf8_lossy(&head_bytes).into_owned();

    let tail = if size <= LITE_READ_BUF_SIZE {
        head.clone()
    } else {
        let tail_offset = size - LITE_READ_BUF_SIZE;
        if let Err(e) = file.seek(SeekFrom::Start(tail_offset)) {
            tracing::debug!(target: crate::logging::targets::CATALOG_SCAN, path = %path.display(), error = %e, step = "seek_tail", "lite-read failed");
            return None;
        }
        let mut tail_bytes = vec![0u8; usize::try_from(LITE_READ_BUF_SIZE).unwrap_or(usize::MAX)];
        let read = match file.read(&mut tail_bytes) {
            Ok(n) => n,
            Err(e) => {
                tracing::debug!(target: crate::logging::targets::CATALOG_SCAN, path = %path.display(), error = %e, step = "read_tail", "lite-read failed");
                return None;
            }
        };
        tail_bytes.truncate(read);
        String::from_utf8_lossy(&tail_bytes).into_owned()
    };

    let tag = match file.seek(SeekFrom::Start(0)) {
        Ok(_) => find_session_tag(BufReader::new(&mut file)),
        Err(e) => {
            tracing::debug!(target: crate::logging::targets::CATALOG_SCAN, path = %path.display(), error = %e, step = "seek_tag_scan", "lite-read tag-scan failed; resume will treat as untagged");
            None
        }
    };

    Some(LiteSessionFile { mtime, size, head, tail, tag })
}

/// Line-iterate a JSONL stream and return the value of the LAST
/// `{"type":"tag"}` row's `"tag"` field. Empty strings are filtered.
/// Returns `None` when no tag row is present or any read errors out.
///
/// Last-wins semantics preserve PR #167: a `/new` re-tag appended
/// later in the transcript supersedes the original spawn tag.
fn find_session_tag<R: BufRead>(reader: R) -> Option<String> {
    let mut last_tag: Option<String> = None;
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                tracing::debug!(target: crate::logging::targets::CATALOG_SCAN, error = %e, step = "tag_scan_line", "lite-read tag-scan line read failed; ending scan with last seen tag");
                break;
            }
        };
        if !line.starts_with("{\"type\":\"tag\"") {
            continue;
        }
        if let Some(tag) = extract_last_json_string_field(&line, "tag")
            && !tag.is_empty()
        {
            last_tag = Some(tag);
        }
    }
    last_tag
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
    // Track the byte offset of the winning match: the compact and
    // spaced patterns are scanned separately, so without comparing
    // positions the spaced scan would clobber a later compact match.
    let mut last_pos: Option<usize> = None;
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
                    if last_pos.is_none_or(|p| idx >= p)
                        && let Ok(raw) = std::str::from_utf8(&bytes[value_start..i])
                    {
                        last = Some(unescape_json_string(raw));
                        last_pos = Some(idx);
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
/// patterns the CLI's `_SKIP_FIRST_PROMPT_PATTERN` matches. Truncates to
/// 200 chars with an ellipsis.
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

/// Fixed-prefix counterpart to the CLI's `_SKIP_FIRST_PROMPT_PATTERN`.
pub(crate) fn should_skip_first_prompt(s: &str) -> bool {
    const PREFIXES: [&str; 4] =
        ["<local-command-stdout>", "<session-start-hook>", "<tick>", "<goal>"];
    if PREFIXES.iter().any(|p| s.starts_with(p)) {
        return true;
    }
    if s.starts_with("[Request interrupted by user") && s.contains(']') {
        return true;
    }
    let trimmed = s.trim();
    for (open, close) in
        [("<ide_opened_file>", "</ide_opened_file>"), ("<ide_selection>", "</ide_selection>")]
    {
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

/// Build an [`SDKSessionInfo`] from a lite head/tail read. Skips
/// sidechain transcripts and metadata-only sessions (no summary
/// after all fallbacks).
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
    // Bias toward labels that identify what the session is about, not
    // what was last said in it. customTitle / aiTitle (claude-written
    // 4-6 word title) is best; first_prompt (the user's opening
    // prompt) is the next-best identifier; lastPrompt (a mid-session
    // follow-up that needs context to interpret) is the fallback
    // because it produces unreadable labels at narrow widths.
    let summary = custom_title
        .clone()
        .or_else(|| first_prompt.clone())
        .or_else(|| extract_last_json_string_field(tail, "lastPrompt"))
        .or_else(|| extract_last_json_string_field(tail, "summary"))?;

    let git_branch = extract_last_json_string_field(tail, "gitBranch")
        .or_else(|| extract_json_string_field(head, "gitBranch"));
    let cwd = extract_json_string_field(head, "cwd").or_else(|| project_path.map(str::to_string));
    // Tag is pre-extracted by `read_session_lite` via a full-file
    // line-scan; neither head nor tail alone covers every position
    // a `{"type":"tag"}` row can land at.
    let tag = lite.tag.clone();
    let created_at = extract_json_string_field(first_line, "timestamp")
        .and_then(|ts| parse_rfc3339_ms(&ts).ok());

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

/// Parse the CLI's RFC-3339 timestamp (e.g. `2026-04-22T04:15:27.123Z`)
/// into Unix epoch milliseconds. Sub-millisecond precision is truncated.
pub(crate) fn parse_rfc3339_ms(ts: &str) -> Result<u64, time::error::Parse> {
    let dt = time::OffsetDateTime::parse(ts, &time::format_description::well_known::Rfc3339)?;
    let nanos = dt.unix_timestamp_nanos();
    Ok(u64::try_from(nanos / 1_000_000).unwrap_or(0))
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn sanitize_ascii_only_passthrough() {
        assert_eq!(sanitize_path("alphanum123"), "alphanum123");
    }

    #[test]
    fn sanitize_replaces_non_alphanum_with_hyphens() {
        assert_eq!(
            sanitize_path("/Users/dev/projects/forge"),
            "-Users-dev-projects-forge"
        );
    }

    #[test]
    fn extract_last_json_string_field_picks_globally_last_across_forms() {
        // The compact form appears LATER than the spaced form; the
        // globally-last value must win regardless of which pattern is
        // scanned first (the bug was the spaced scan clobbering a later
        // compact match).
        let text = r#"{"tag": "early-spaced"} {"tag":"late-compact"}"#;
        assert_eq!(extract_last_json_string_field(text, "tag"), Some("late-compact".to_owned()));
        // Reverse: spaced later than compact.
        let text2 = r#"{"tag":"early-compact"} {"tag": "late-spaced"}"#;
        assert_eq!(extract_last_json_string_field(text2, "tag"), Some("late-spaced".to_owned()));
    }

    #[test]
    fn simple_hash_matches_known_value() {
        // Reference: _simple_hash("foo") → "26di" (computed from the
        // same 32-bit JS-style hash algorithm).
        assert_eq!(simple_hash("foo"), "26di");
    }

    #[test]
    fn long_path_gets_hash_suffix() {
        let long = "a".repeat(300);
        let got = sanitize_path(&long);
        assert_eq!(got.len(), MAX_SANITIZED_LENGTH + 1 + simple_hash(&long).len());
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
        let ms = parse_rfc3339_ms("2026-04-22T00:00:00.500Z").unwrap();
        assert_eq!(ms % 1000, 500);
    }

    #[test]
    fn extract_json_string_field_finds_compact_form() {
        let t = r#"noise {"type":"user","message":{"content":"hi"}} noise"#;
        assert_eq!(extract_json_string_field(t, "content"), Some("hi".to_string()));
    }

    #[test]
    fn extract_json_string_field_finds_spaced_form() {
        let t = r#"{"gitBranch": "main"}"#;
        assert_eq!(extract_json_string_field(t, "gitBranch"), Some("main".to_string()));
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
        assert_eq!(extract_last_json_string_field(t, "tag"), Some("new".to_string()));
    }

    #[test]
    fn first_prompt_skips_local_command_stdout() {
        let head = r#"{"type":"user","message":{"content":"<local-command-stdout>out</local-command-stdout>"}}
{"type":"user","message":{"content":"actual prompt"}}"#;
        assert_eq!(extract_first_prompt_from_head(head), Some("actual prompt".to_string()));
    }

    #[test]
    fn first_prompt_falls_back_to_command_name() {
        let head = r#"{"type":"user","message":{"content":"<command-name>foo</command-name>"}}"#;
        assert_eq!(extract_first_prompt_from_head(head), Some("foo".to_string()));
    }

    #[test]
    fn first_prompt_skips_tool_result_line() {
        let head = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"x"}]}}
{"type":"user","message":{"content":"real prompt"}}"#;
        assert_eq!(extract_first_prompt_from_head(head), Some("real prompt".to_string()));
    }

    #[test]
    fn parse_session_info_skips_sidechain() {
        let head = "{\"isSidechain\":true,\"type\":\"user\"}\n".to_string();
        let lite = LiteSessionFile { mtime: 0, size: 1, head: head.clone(), tail: head, tag: None };
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
            tag: Some("meta".to_string()),
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
            tag: Some("mytag".to_string()),
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
    fn find_session_tag_ignores_tag_on_tool_use_lines() {
        // A git-tag tool_use shouldn't be picked up as a session tag -
        // the `"tag"` string appears but the line isn't `{"type":"tag"`.
        let content = r#"{"type":"user","message":{"content":"hi"}}
{"type":"assistant","message":{"content":[{"type":"tool_use","input":{"command":"git tag","tag":"v1.0"}}]}}
"#;
        assert_eq!(find_session_tag(std::io::Cursor::new(content)), None);
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
            tag: None,
        };
        let info = parse_session_info_from_lite("abc", &lite, None).expect("some");
        assert_eq!(info.summary, "Curated");
        assert_eq!(info.custom_title.as_deref(), Some("Curated"));
    }

    // ---------------------------------------------------------------------
    // Subagent-listing helpers - the recursive walk + filename filter
    //
    //.
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
        assert!(!ids.contains(&"bbb"), "agent-bbb.txt must be ignored (wrong extension)");
        assert!(!ids.contains(&"random"), "random.jsonl must be ignored (missing `agent-` prefix)");
    }

    #[test]
    fn collect_agent_files_recurses_into_nested_subdirs() {
        // The CLI writes subagents at `workflows/<run_id>/agent-<id>.jsonl`
        //. Walk must find them.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        write_tmp_file(&base.join("workflows").join("run1").join("agent-nested.jsonl"), "{}\n");
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
    fn parse_session_messages_hoists_attachment_queued_command_to_user() {
        // claude persists mid-turn queued inputs as
        // `{"type":"attachment", "attachment":{"type":"queued_command",
        // "prompt":"...", "commandMode":"prompt"}}`. The scanner must
        // synthesise these as user rows so replay reconstructs the
        // bubble that was never on the live wire as a regular user
        // message.
        let jsonl = r#"{"type":"user","message":{"role":"user","content":"plain prompt"},"uuid":"u1","session_id":"s1"}
{"type":"attachment","attachment":{"type":"queued_command","prompt":"queued prompt","commandMode":"prompt"},"uuid":"a1","session_id":"s1"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hello"}]},"uuid":"as1","session_id":"s1"}
"#;
        let msgs = parse_session_messages(jsonl.as_bytes());
        assert_eq!(msgs.len(), 3, "expected 3 rows (user + synthesised user + assistant)");
        assert!(matches!(msgs[0].kind, SessionMessageKind::User));
        assert!(matches!(msgs[1].kind, SessionMessageKind::User), "attachment row hoisted to User");
        assert_eq!(msgs[1].uuid, "a1");
        let content = msgs[1]
            .message
            .get("content")
            .and_then(Value::as_array)
            .expect("synthesised message has content array");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].get("type").and_then(Value::as_str), Some("queued_command"));
        assert_eq!(content[0].get("prompt").and_then(Value::as_str), Some("queued prompt"));
        assert_eq!(content[0].get("commandMode").and_then(Value::as_str), Some("prompt"));
        assert!(matches!(msgs[2].kind, SessionMessageKind::Assistant));
    }

    #[test]
    fn parse_session_messages_skips_attachment_without_queued_command() {
        // Other attachment subtypes (image, document, etc.) are not
        // user-bubble-worthy - keep skipping them.
        let jsonl = r#"{"type":"attachment","attachment":{"type":"image","source":{}},"uuid":"a1","session_id":"s1"}
{"type":"assistant","message":{"role":"assistant","content":[]},"uuid":"as1","session_id":"s1"}
"#;
        let msgs = parse_session_messages(jsonl.as_bytes());
        assert_eq!(msgs.len(), 1, "non-queued_command attachment must be skipped");
        assert!(matches!(msgs[0].kind, SessionMessageKind::Assistant));
    }

    fn session_info_with_tag(session_id: &str, tag: Option<&str>) -> SDKSessionInfo {
        SDKSessionInfo {
            session_id: session_id.to_string(),
            summary: "test".to_string(),
            last_modified: 0,
            file_size: None,
            custom_title: None,
            first_prompt: None,
            git_branch: None,
            cwd: None,
            tag: tag.map(str::to_string),
            created_at: None,
        }
    }

    #[test]
    fn parse_session_info_filter_excludes_worker_tag_by_default() {
        let content = "{\"type\":\"user\",\"timestamp\":\"2026-04-22T00:00:00.000Z\",\"message\":{\"content\":\"hi\"}}\n{\"type\":\"tag\",\"tag\":\"forge:worker:reviewer\"}\n".to_string();
        let lite = LiteSessionFile {
            mtime: 0,
            size: content.len() as u64,
            head: content.clone(),
            tail: content,
            tag: Some("forge:worker:reviewer".to_string()),
        };
        let info = parse_session_info_from_lite("abc", &lite, None).expect("some");
        assert_eq!(info.tag.as_deref(), Some("forge:worker:reviewer"));
        // The session is still parseable - filtering happens at the
        // list_sessions caller layer, not here.
        assert!(should_exclude_worker_tag(&info));
    }

    #[test]
    fn should_exclude_worker_tag_recognises_prefix() {
        let info = session_info_with_tag("s1", Some("forge:worker:reviewer"));
        assert!(should_exclude_worker_tag(&info));
    }

    #[test]
    fn should_exclude_worker_tag_passes_lead_and_untagged() {
        let lead = session_info_with_tag("s1", Some("forge:lead"));
        let untagged = session_info_with_tag("s2", None);
        assert!(!should_exclude_worker_tag(&lead));
        assert!(!should_exclude_worker_tag(&untagged));
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

    // -----------------------------------------------------------------
    // Tag-scan regression tests.
    //
    // The tag row is written ONCE at session start and persists for the
    // life of the JSONL; PR #167's `/new` flow can re-write a tag
    // mid-file. The lite head/tail window misses both cases on large
    // transcripts. These tests exercise the full-file scan path via
    // `read_session_info`.
    // -----------------------------------------------------------------

    fn write_session_jsonl(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(format!("{name}.jsonl"));
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn read_session_info_finds_tag_at_start_of_large_file() {
        // The bug shape: tag row is written at session start and sits
        // permanently near byte 0. Tail-only extraction scans only
        // the last LITE_READ_BUF_SIZE bytes, so for any transcript
        // > ~130 KB the tag falls outside the tail window and resume
        // sees `info.tag = None`. Build a transcript well past two
        // window-widths so the bug is unambiguous.
        let window = usize::try_from(LITE_READ_BUF_SIZE).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let mut body = String::new();
        body.push_str(
            "{\"type\":\"user\",\"timestamp\":\"2026-04-22T00:00:00.000Z\",\"message\":{\"content\":\"hello\"}}\n",
        );
        body.push_str("{\"type\":\"tag\",\"tag\":\"forge:worker:reviewer\"}\n");
        let filler = "{\"type\":\"assistant\",\"message\":{\"content\":\"x\"}}\n";
        while body.len() < window * 3 {
            body.push_str(filler);
        }
        let path = write_session_jsonl(tmp.path(), "abc", &body);

        let info = read_session_info(&path).expect("session info parsed");
        assert_eq!(info.tag.as_deref(), Some("forge:worker:reviewer"));
    }

    #[test]
    fn read_session_info_finds_tag_mid_file() {
        // The `/new` case (PR #167): user fires `/new` mid-session and
        // the CLI appends a fresh tag row at the current cursor. For a
        // large transcript that's neither head nor tail, it lands in
        // the middle, where neither window sees it.
        let window = usize::try_from(LITE_READ_BUF_SIZE).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let mut body = String::new();
        body.push_str(
            "{\"type\":\"user\",\"timestamp\":\"2026-04-22T00:00:00.000Z\",\"message\":{\"content\":\"hello\"}}\n",
        );
        let filler = "{\"type\":\"assistant\",\"message\":{\"content\":\"x\"}}\n";
        // Push past the head window with filler, then plant the tag.
        while body.len() < window + 4_096 {
            body.push_str(filler);
        }
        body.push_str("{\"type\":\"tag\",\"tag\":\"forge:worker:tester\"}\n");
        // Then push past the tail window with more filler so the tag
        // sits in the middle region neither head nor tail covers.
        while body.len() < window * 3 {
            body.push_str(filler);
        }
        let path = write_session_jsonl(tmp.path(), "abc", &body);

        let info = read_session_info(&path).expect("session info parsed");
        assert_eq!(info.tag.as_deref(), Some("forge:worker:tester"));
    }

    #[test]
    fn read_session_info_picks_last_tag_when_multiple() {
        // Preserve PR #167 semantics: when the JSONL carries multiple
        // tag rows (original spawn tag + later `/new` re-tag), the
        // most recent one wins. File must be larger than two window
        // widths so neither tag lands in head or tail, otherwise the
        // old tail-only path would coincidentally pick the right one
        // and mask a behavioural regression.
        let window = usize::try_from(LITE_READ_BUF_SIZE).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let mut body = String::new();
        body.push_str(
            "{\"type\":\"user\",\"timestamp\":\"2026-04-22T00:00:00.000Z\",\"message\":{\"content\":\"hi\"}}\n",
        );
        body.push_str("{\"type\":\"tag\",\"tag\":\"first\"}\n");
        let filler = "{\"type\":\"assistant\",\"message\":{\"content\":\"x\"}}\n";
        while body.len() < window + 4_096 {
            body.push_str(filler);
        }
        body.push_str("{\"type\":\"tag\",\"tag\":\"second\"}\n");
        while body.len() < window * 3 {
            body.push_str(filler);
        }
        let path = write_session_jsonl(tmp.path(), "abc", &body);

        let info = read_session_info(&path).expect("session info parsed");
        assert_eq!(info.tag.as_deref(), Some("second"));
    }
}
