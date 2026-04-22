//! Incremental session-summary derivation for `SessionStore` adapters.
//!
//! [`fold_session_summary`] lets a store maintain a per-session
//! [`SessionSummaryEntry`] sidecar incrementally inside `append()` so
//! `list_sessions_from_store()` can fetch all metadata in a single
//! `list_session_summaries()` call instead of N per-session `load()`
//! calls.
//!
//! Every derived field is append-incremental (set-once or last-wins)
//! so adapters never need to re-read previously appended entries.
//!
//! Ports Python SDK v0.1.64 `_internal/session_summary.py`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::public_types::SDKSessionInfo;
use crate::session::scan::{chrono_like_parse_ms, extract_command_name, should_skip_first_prompt};
use crate::session::store::{SessionKey, SessionStoreEntry};

/// Incrementally-maintained session summary. Mirrors Python
/// `SessionSummaryEntry` (`types.py:1209`).
///
/// Stores obtain this from [`fold_session_summary`] inside
/// [`SessionStore::append`](crate::SessionStore::append) and persist
/// it verbatim; they return the full set from
/// `SessionStore::list_session_summaries`. The `data` field is opaque
/// SDK-owned state — stores MUST NOT interpret it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummaryEntry {
    /// Session UUID.
    pub session_id: String,
    /// Storage write time of the sidecar, in Unix epoch milliseconds.
    ///
    /// Must use the same clock source as the `mtime` returned by
    /// [`SessionStore::list_sessions`](crate::SessionStore::list_sessions)
    /// for this session — typically file mtime, S3 `LastModified`,
    /// Postgres `updated_at`, or whatever native timestamp the
    /// adapter surfaces. Do NOT derive this from entry ISO timestamps:
    /// adapters that write in batches would report storage times
    /// strictly later than the last entry's timestamp, making every
    /// sidecar appear stale and defeating the fast-path staleness
    /// check in `list_sessions_from_store`. [`fold_session_summary`]
    /// preserves whatever `mtime` the caller passes in via `prev` and
    /// does not set it itself; stamp it after persisting.
    pub mtime: u64,
    /// Opaque SDK-owned summary state. Persist verbatim; do not
    /// interpret.
    pub data: Map<String, Value>,
}

/// Fold a batch of appended entries into the running summary for `key`.
///
/// Stores call this from inside `append()` to keep a
/// [`SessionSummaryEntry`] sidecar up to date without re-reading the
/// transcript. `prev` is the previous summary for the same key (or
/// `None` for the first append).
///
/// Do not call this for keys with a `subpath` — subagent transcripts
/// must not contribute to the main session's summary. Guard with
/// `if key.subpath.is_none()` before calling.
///
/// All derived state lives in the opaque `data` map; stores persist
/// it verbatim and do not interpret it.
///
/// `mtime` is NOT touched by the fold — it is the sidecar's storage
/// write time and must be stamped by the adapter after persisting.
/// For a new session (`prev.is_none()`) the fold returns `mtime = 0`
/// as a placeholder; the adapter is expected to overwrite it.
#[must_use]
pub fn fold_session_summary(
    prev: Option<&SessionSummaryEntry>,
    key: &SessionKey,
    entries: &[SessionStoreEntry],
) -> SessionSummaryEntry {
    let mut summary = match prev {
        Some(p) => SessionSummaryEntry {
            session_id: p.session_id.clone(),
            mtime: p.mtime,
            data: p.data.clone(),
        },
        None => SessionSummaryEntry {
            session_id: key.session_id.clone(),
            mtime: 0,
            data: Map::new(),
        },
    };

    for entry in entries {
        fold_one(&mut summary.data, entry);
    }

    summary
}

/// Convert a [`SessionSummaryEntry`] to [`SDKSessionInfo`]. Mirrors
/// Python `summary_entry_to_sdk_info` (`session_summary.py:193-233`).
///
/// Returns `None` for sidechain sessions or sessions with no
/// extractable summary, matching
/// [`_parse_session_info_from_lite`](crate::session::scan)'s
/// filtering.
#[must_use]
pub fn summary_entry_to_sdk_info(
    entry: &SessionSummaryEntry,
    project_path: Option<&str>,
) -> Option<SDKSessionInfo> {
    let data = &entry.data;
    if data
        .get("is_sidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }

    let first_prompt_locked = data
        .get("first_prompt_locked")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let first_prompt: Option<String> = if first_prompt_locked {
        data.get("first_prompt")
            .and_then(Value::as_str)
            .map(ToString::to_string)
    } else {
        data.get("command_fallback")
            .and_then(Value::as_str)
            .map(ToString::to_string)
    };

    let custom_title = data
        .get("custom_title")
        .and_then(Value::as_str)
        .or_else(|| data.get("ai_title").and_then(Value::as_str))
        .map(ToString::to_string);

    let summary = custom_title
        .clone()
        .or_else(|| {
            data.get("last_prompt")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .or_else(|| {
            data.get("summary_hint")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .or_else(|| first_prompt.clone())?;

    Some(SDKSessionInfo {
        session_id: entry.session_id.clone(),
        summary,
        last_modified: entry.mtime,
        // file_size is a JSONL byte count — meaningful only for the
        // local-disk path. Stores have no equivalent.
        file_size: None,
        custom_title,
        first_prompt,
        git_branch: data
            .get("git_branch")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        cwd: data
            .get("cwd")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .or_else(|| project_path.map(ToString::to_string)),
        tag: data
            .get("tag")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        created_at: data.get("created_at").and_then(Value::as_u64),
    })
}

// ---------------------------------------------------------------------
// Private helpers — one-entry fold + field extraction.
// ---------------------------------------------------------------------

const LAST_WINS_FIELDS: &[(&str, &str)] = &[
    ("customTitle", "custom_title"),
    ("aiTitle", "ai_title"),
    ("lastPrompt", "last_prompt"),
    ("summary", "summary_hint"),
    ("gitBranch", "git_branch"),
];

fn fold_one(data: &mut Map<String, Value>, entry: &SessionStoreEntry) {
    let entry_obj = entry_as_object(entry);

    let ms = entry
        .timestamp
        .as_deref()
        .and_then(|ts| chrono_like_parse_ms(ts).ok());

    if !data.contains_key("is_sidechain") {
        let is_side = entry_obj
            .and_then(|m| m.get("isSidechain"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        data.insert("is_sidechain".into(), Value::Bool(is_side));
    }
    if !data.contains_key("created_at") {
        if let Some(ms) = ms {
            data.insert("created_at".into(), Value::from(ms));
        }
    }
    if !data.contains_key("cwd") {
        if let Some(cwd) = entry_obj.and_then(|m| m.get("cwd")).and_then(Value::as_str) {
            if !cwd.is_empty() {
                data.insert("cwd".into(), Value::String(cwd.into()));
            }
        }
    }

    fold_first_prompt(data, entry);

    for (src, dst) in LAST_WINS_FIELDS {
        if let Some(val) = entry_obj.and_then(|m| m.get(*src)).and_then(Value::as_str) {
            data.insert((*dst).into(), Value::String(val.into()));
        }
    }

    if entry.ty == "tag" {
        let tag_val = entry_obj
            .and_then(|m| m.get("tag"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if tag_val.is_empty() {
            data.remove("tag");
        } else {
            data.insert("tag".into(), Value::String(tag_val.into()));
        }
    }
}

/// Access the `SessionStoreEntry`'s fields as a flat JSON object. The
/// entry's extras live on `.extra`; the typed fields (`type`, `uuid`,
/// `timestamp`) live on the struct. Most folded-in field lookups
/// (`customTitle`, `gitBranch`, `cwd`, `isSidechain`, `tag`, `message`)
/// are extras, so returning the extras map is sufficient for the fold.
fn entry_as_object(entry: &SessionStoreEntry) -> Option<&Map<String, Value>> {
    entry.extra.as_object()
}

/// Replicate Python's `_fold_first_prompt` for a single parsed entry.
/// Mutates `data` in place.
fn fold_first_prompt(data: &mut Map<String, Value>, entry: &SessionStoreEntry) {
    if data
        .get("first_prompt_locked")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return;
    }
    if entry.ty != "user" {
        return;
    }
    let Some(extras) = entry.extra.as_object() else {
        return;
    };
    if extras
        .get("isMeta")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return;
    }
    if extras
        .get("isCompactSummary")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return;
    }

    let Some(message) = extras.get("message") else {
        return;
    };
    let content = message.get("content");

    // Skip tool_result-carrying user messages.
    if let Some(arr) = content.and_then(Value::as_array) {
        let has_tool_result = arr.iter().any(|b| {
            b.as_object()
                .and_then(|m| m.get("type"))
                .and_then(Value::as_str)
                == Some("tool_result")
        });
        if has_tool_result {
            return;
        }
    }

    let texts: Vec<String> = if let Some(s) = content.and_then(Value::as_str) {
        vec![s.to_string()]
    } else if let Some(arr) = content.and_then(Value::as_array) {
        arr.iter()
            .filter_map(|b| {
                let obj = b.as_object()?;
                if obj.get("type").and_then(Value::as_str) != Some("text") {
                    return None;
                }
                obj.get("text")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
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
            if !data.contains_key("command_fallback") {
                data.insert("command_fallback".into(), Value::String(cmd));
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
        data.insert("first_prompt".into(), Value::String(truncated));
        data.insert("first_prompt_locked".into(), Value::Bool(true));
        return;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use serde_json::json;

    fn key() -> SessionKey {
        SessionKey {
            project_key: "p".into(),
            session_id: "abc".into(),
            subpath: None,
        }
    }

    fn user_entry(extras: Value) -> SessionStoreEntry {
        SessionStoreEntry {
            ty: "user".into(),
            uuid: None,
            timestamp: extras
                .get("timestamp")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            extra: extras,
        }
    }

    #[test]
    fn fold_new_session_initialises_summary() {
        let entry = user_entry(json!({
            "timestamp": "2026-04-22T00:00:00.000Z",
            "isSidechain": false,
            "cwd": "/home/user/project",
            "message": {"content": "hello"},
        }));
        let summary = fold_session_summary(None, &key(), &[entry]);
        assert_eq!(summary.session_id, "abc");
        assert_eq!(summary.mtime, 0, "fold must not touch mtime");
        assert_eq!(
            summary.data.get("is_sidechain").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            summary.data.get("cwd").and_then(Value::as_str),
            Some("/home/user/project")
        );
        assert_eq!(
            summary.data.get("first_prompt").and_then(Value::as_str),
            Some("hello")
        );
        assert_eq!(
            summary
                .data
                .get("first_prompt_locked")
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn fold_preserves_prev_mtime() {
        let prev = SessionSummaryEntry {
            session_id: "abc".into(),
            mtime: 12_345,
            data: Map::new(),
        };
        let entry = user_entry(json!({"message": {"content": "hi"}}));
        let summary = fold_session_summary(Some(&prev), &key(), &[entry]);
        assert_eq!(summary.mtime, 12_345);
    }

    #[test]
    fn fold_first_prompt_locks_after_first_real_entry() {
        let first = user_entry(json!({"message": {"content": "first"}}));
        let second = user_entry(json!({"message": {"content": "second"}}));
        let s1 = fold_session_summary(None, &key(), &[first]);
        let s2 = fold_session_summary(Some(&s1), &key(), &[second]);
        assert_eq!(
            s2.data.get("first_prompt").and_then(Value::as_str),
            Some("first"),
            "first_prompt must latch on the first match"
        );
    }

    #[test]
    fn fold_skips_tool_result_user_messages() {
        let entry = user_entry(json!({
            "message": {
                "content": [{"type": "tool_result", "tool_use_id": "t", "content": "x"}]
            }
        }));
        let s = fold_session_summary(None, &key(), &[entry]);
        assert!(s.data.get("first_prompt").is_none());
    }

    #[test]
    fn fold_tag_entry_sets_tag() {
        let entry = SessionStoreEntry {
            ty: "tag".into(),
            uuid: None,
            timestamp: None,
            extra: json!({"tag": "v1.0"}),
        };
        let s = fold_session_summary(None, &key(), &[entry]);
        assert_eq!(s.data.get("tag").and_then(Value::as_str), Some("v1.0"));
    }

    #[test]
    fn fold_empty_tag_clears_previous() {
        let set = SessionStoreEntry {
            ty: "tag".into(),
            uuid: None,
            timestamp: None,
            extra: json!({"tag": "v1"}),
        };
        let clear = SessionStoreEntry {
            ty: "tag".into(),
            uuid: None,
            timestamp: None,
            extra: json!({"tag": ""}),
        };
        let s1 = fold_session_summary(None, &key(), &[set]);
        let s2 = fold_session_summary(Some(&s1), &key(), &[clear]);
        assert!(s2.data.get("tag").is_none());
    }

    #[test]
    fn fold_last_wins_fields_overwrite() {
        let first = user_entry(json!({
            "customTitle": "First title",
            "message": {"content": "x"},
        }));
        let second = user_entry(json!({
            "customTitle": "Second title",
            "message": {"content": "y"},
        }));
        let s1 = fold_session_summary(None, &key(), &[first]);
        let s2 = fold_session_summary(Some(&s1), &key(), &[second]);
        assert_eq!(
            s2.data.get("custom_title").and_then(Value::as_str),
            Some("Second title")
        );
    }

    #[test]
    fn fold_sidechain_set_once() {
        let first = user_entry(json!({"isSidechain": true, "message": {"content": "x"}}));
        let second = user_entry(json!({"isSidechain": false, "message": {"content": "y"}}));
        let s = fold_session_summary(None, &key(), &[first, second]);
        assert_eq!(
            s.data.get("is_sidechain").and_then(Value::as_bool),
            Some(true),
            "is_sidechain must set-once (first entry wins)"
        );
    }

    #[test]
    fn summary_entry_to_sdk_info_skips_sidechain() {
        let mut data = Map::new();
        data.insert("is_sidechain".into(), Value::Bool(true));
        let entry = SessionSummaryEntry {
            session_id: "abc".into(),
            mtime: 0,
            data,
        };
        assert!(summary_entry_to_sdk_info(&entry, None).is_none());
    }

    #[test]
    fn summary_entry_to_sdk_info_custom_title_wins() {
        let mut data = Map::new();
        data.insert("custom_title".into(), Value::String("Curated".into()));
        data.insert("last_prompt".into(), Value::String("Last".into()));
        let entry = SessionSummaryEntry {
            session_id: "abc".into(),
            mtime: 42,
            data,
        };
        let info = summary_entry_to_sdk_info(&entry, None).expect("some");
        assert_eq!(info.summary, "Curated");
        assert_eq!(info.custom_title.as_deref(), Some("Curated"));
        assert_eq!(info.last_modified, 42);
    }

    #[test]
    fn summary_entry_to_sdk_info_cwd_falls_back_to_project_path() {
        let data = Map::new();
        let entry = SessionSummaryEntry {
            session_id: "abc".into(),
            mtime: 0,
            data,
        };
        // No summary → None.
        assert!(summary_entry_to_sdk_info(&entry, Some("/p")).is_none());
    }

    #[test]
    fn fold_command_fallback_captures_slash_command() {
        let entry = user_entry(json!({
            "message": {"content": "<command-name>/foo</command-name>"}
        }));
        let s = fold_session_summary(None, &key(), &[entry]);
        assert_eq!(
            s.data.get("command_fallback").and_then(Value::as_str),
            Some("/foo")
        );
        assert!(s.data.get("first_prompt").is_none());
    }

    #[test]
    fn fold_skips_local_command_stdout() {
        let entry = user_entry(json!({
            "message": {"content": "<local-command-stdout>out</local-command-stdout>"}
        }));
        let s = fold_session_summary(None, &key(), &[entry]);
        assert!(s.data.get("first_prompt").is_none());
    }

    #[test]
    fn fold_truncates_long_prompts() {
        let long: String = "a".repeat(300);
        let entry = user_entry(json!({"message": {"content": long}}));
        let s = fold_session_summary(None, &key(), &[entry]);
        let prompt = s
            .data
            .get("first_prompt")
            .and_then(Value::as_str)
            .expect("prompt");
        assert_eq!(prompt.chars().count(), 201); // 200 chars + ellipsis
        assert!(prompt.ends_with('\u{2026}'));
    }
}
