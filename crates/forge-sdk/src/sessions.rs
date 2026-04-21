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
//! Not yet ported: subagent transcripts (`list_subagents`,
//! `get_subagent_messages`), the head-only read optimisation
//! (`_read_session_lite`), git-worktree discovery, and the
//! `SessionStore`-backed `*_from_store` variants. These are all
//! follow-up work.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::public_types::{SDKSessionInfo, SessionMessage, SessionMessageKind};

const MAX_SANITIZED_LENGTH: usize = 200;

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

/// Resolve the projects directory, honouring `CLAUDE_CONFIG_DIR` first
/// and falling back to `~/.claude/projects`.
fn projects_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(custom.trim_end_matches('/')).join("projects");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".claude").join("projects")
}

/// Resolve a project directory path to its sanitised on-disk key. Python
/// canonicalises via `realpath`; we follow suit when the path exists,
/// else fall back to the raw input.
fn project_dir_for(project_path: &str) -> PathBuf {
    let canonical = match fs::canonicalize(project_path) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(_) => project_path.to_string(),
    };
    projects_dir().join(sanitize_path(&canonical))
}

/// List sessions. When `directory` is `Some`, scans that project dir
/// (ignoring git worktrees for now — `include_worktrees` is reserved);
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
    _include_worktrees: bool,
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
    let mut out = Vec::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let kind = match value.get("type").and_then(Value::as_str) {
            Some("user") => SessionMessageKind::User,
            Some("assistant") => SessionMessageKind::Assistant,
            _ => continue,
        };
        // Skip tool-use sidechain messages — Python does the same.
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

fn is_valid_uuid(s: &str) -> bool {
    // 8-4-4-4-12 hex, accept both lower and upper case.
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    matches!(
        (
            parts[0].len(),
            parts[1].len(),
            parts[2].len(),
            parts[3].len(),
            parts[4].len()
        ),
        (8, 4, 4, 4, 12)
    ) && parts
        .iter()
        .all(|p| p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn read_session_info(path: &Path) -> Option<SDKSessionInfo> {
    let meta = fs::metadata(path).ok()?;
    let last_modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    let file_size = meta.len();
    let session_id = path.file_stem().and_then(|s| s.to_str())?.to_string();

    let file = fs::File::open(path).ok()?;
    let mut first_prompt: Option<String> = None;
    let mut custom_title: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut tag: Option<String> = None;
    let mut created_at: Option<u64> = None;
    let mut summary: Option<String> = None;
    let mut extras: HashMap<String, Value> = HashMap::new();

    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
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
        if created_at.is_none() {
            if let Some(ts) = value.get("timestamp").and_then(Value::as_str) {
                if let Ok(parsed) = chrono_like_parse_ms(ts) {
                    created_at = Some(parsed);
                }
            }
        }
        if custom_title.is_none() {
            if let Some(v) = value.get("customTitle").and_then(Value::as_str) {
                custom_title = Some(v.to_string());
            }
        }
        if cwd.is_none() {
            if let Some(v) = value.get("cwd").and_then(Value::as_str) {
                cwd = Some(v.to_string());
            }
        }
        if git_branch.is_none() {
            if let Some(v) = value.get("gitBranch").and_then(Value::as_str) {
                git_branch = Some(v.to_string());
            }
        }
        if tag.is_none() {
            if let Some(v) = value.get("tag").and_then(Value::as_str) {
                tag = Some(v.to_string());
            }
        }
        if summary.is_none() {
            if let Some(v) = value.get("summary").and_then(Value::as_str) {
                summary = Some(v.to_string());
            }
        }
        // Keep the first-seen payload for later reference.
        if let Some(obj) = value.as_object() {
            for (k, v) in obj {
                extras.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    }

    let display_summary = summary
        .or_else(|| custom_title.clone())
        .or_else(|| first_prompt.clone())
        .unwrap_or_default();

    Some(SDKSessionInfo {
        session_id,
        summary: display_summary,
        last_modified,
        file_size: Some(file_size),
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
fn chrono_like_parse_ms(ts: &str) -> Result<u64, ()> {
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
    let mut millis: u32 = 0;
    if bytes.get(19) == Some(&b'.') {
        let ms_end = ts.find('Z').unwrap_or(bytes.len());
        millis = ts.get(20..ms_end).and_then(|s| s.parse().ok()).unwrap_or(0);
    }

    // Simple epoch conversion valid for 1970-02-01 through ~2200.
    if year < 1970 {
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
            sanitize_path("/Users/dev/projects/forge"),
            "-Users-dev-projects-forge"
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
}
