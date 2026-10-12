//! Offline session scanners - stateless filesystem helpers that read
//! transcripts from `<config_dir>/projects/<project_key>/*.jsonl`.
//!
//! - [`list_sessions`] - lists sessions, either for one project or all.
//! - [`get_session_messages`] - reads the full transcript for one session.
//!
//! Session metadata ([`list_sessions`]) comes from an internal head +
//! tail lite read, so the fields it parses cost two 64 KiB reads
//! whatever the transcript's size. The worker tag is the exception and
//! is read in full: it can sit anywhere in the file and the last one
//! wins, so there is no window that settles it.

use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use forge_primitives::{
    FORGE_WORKER_TAG_PREFIX, SDKSessionInfo, SessionHistory, SessionMessage, SessionMessageKind,
    worker_tag,
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

fn parse_session_messages<R: std::io::Read>(reader: R) -> SessionHistory {
    let mut out = Vec::new();
    let mut compaction_count = 0_u32;
    for (idx, line_res) in BufReader::new(reader).lines().enumerate() {
        let line = match line_res {
            Ok(l) => l,
            Err(e) => {
                // The compaction count is also truncated here, and it is
                // rendered hidden at zero, so without naming it a torn
                // read is indistinguishable from a session that never
                // compacted. The transcript dir is file-sync replicated,
                // so a torn read is a live case.
                tracing::warn!(
                    line_no = idx,
                    error = %e,
                    compactions_counted_before_truncation = compaction_count,
                    "session scan: read failed; truncating both the message list and the compaction count"
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
        let Some((message, boundary)) = transcript_row(&value) else {
            continue;
        };
        if boundary {
            compaction_count = compaction_count.saturating_add(1);
        }
        out.push(message);
    }
    SessionHistory { messages: out, compaction_count }
}

/// Whether a transcript row carries this frame.
///
/// **The row rule's other half, and the one place it is stated.** A frame
/// whose kind no row round-trips - a `Result`, a thinking-token frame, a
/// `system` subtype other than the two kept - is one the live stream emits and
/// the CLI never writes, so a copy holding it counts a frame the file counts
/// no row for. That difference is what a page below the floor has to be
/// numbered through, by whoever still holds the frames the drops took.
pub fn has_a_transcript_row(message: &forge_primitives::Message) -> bool {
    // The row rule drops a sub-agent's frame, so a frame naming one has no
    // row either, whatever its kind: a numbering that counted it as having
    // one would be off for every page below it.
    let parent = match message {
        forge_primitives::Message::User { parent_tool_use_id, .. }
        | forge_primitives::Message::Assistant { parent_tool_use_id, .. }
        | forge_primitives::Message::StopHookSummary { parent_tool_use_id, .. } => {
            parent_tool_use_id.as_deref()
        }
        _ => None,
    };
    if forge_primitives::names_a_dispatch(parent) {
        return false;
    }
    matches!(
        message,
        forge_primitives::Message::User { .. }
            | forge_primitives::Message::Assistant { .. }
            | forge_primitives::Message::StopHookSummary { .. }
            | forge_primitives::Message::CompactBoundary { .. }
    )
}

/// One transcript row as the session's message, or `None` for a row that is
/// not one. The flag says the row is a compaction boundary, which the read
/// counts.
///
/// **The one row rule, shared by every read of a transcript.** A resume walks
/// the whole file and the paging span read walks a window of it, and a second
/// copy of this is how the two would come to type the same row differently.
fn transcript_row(value: &Value) -> Option<(SessionMessage, bool)> {
    let row_type = value.get("type").and_then(Value::as_str);
    let mut boundary = false;
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
            _ => return None,
        },
        // The only durable record of how often this session has compacted
        // and of where each cut fell - nothing else survives a resume.
        // Counted by the caller, and the row is kept so a resumed conversation
        // draws its boundaries the way a live one does.
        Some("system")
            if value.get("subtype").and_then(Value::as_str) == Some("compact_boundary") =>
        {
            boundary = true;
            // Keyed on what the row yielded rather than on the metadata
            // object being there: the plausible drift is a rename inside it
            // (`preTokens`, per the primitives test), which leaves the
            // object present and one field unread - a row drawn without
            // that fact, and nothing else saying why. The live arm warns on
            // the same degradation. Every modelled field is read here, so a
            // rename of any one of them lands in this record.
            let missing = missing_boundary_facts(value);
            if !missing.is_empty() {
                let uuid = value.get("uuid").and_then(Value::as_str).unwrap_or_default();
                let session = session_in_row(value);
                tracing::warn!(
                    target: "forge_agent::userdata::catalog",
                    event_name = "compact_boundary_missing_facts",
                    uuid,
                    session,
                    missing = %missing.join(", "),
                    "compact_boundary row did not yield every fact; counted, but the kept frame arrives untyped",
                );
            }
            (SessionMessageKind::System, Some(compact_boundary_frame(value)))
        }
        // What the turn's hooks did. A system row is the frame the wire
        // sends, so it is kept whole; the one field the frame's decoder
        // spells differently is the session.
        Some("system")
            if value.get("subtype").and_then(Value::as_str) == Some("stop_hook_summary") =>
        {
            let mut frame = value.clone();
            if let Some(record) = frame.as_object_mut() {
                record.insert("session_id".into(), Value::String(session_in_row(value)));
            }
            (SessionMessageKind::System, Some(frame))
        }
        _ => return None,
    };
    // The same rule the fold reads a frame by: a parent id that names a
    // dispatch, which on the wire is never empty and never null.
    if value
        .get("parent_tool_use_id")
        .and_then(Value::as_str)
        .is_some_and(|parent| !parent.trim().is_empty())
    {
        return None;
    }
    let uuid = value.get("uuid").and_then(Value::as_str).unwrap_or_default().to_string();
    let sess = session_in_row(value);
    let timestamp = value.get("timestamp").and_then(Value::as_str).map(str::to_owned);
    // The CLI's own stamps that nobody typed this row. They do not
    // co-occur - the reminder carries `isMeta` and `turnCompanion`, a
    // compaction summary carries `isCompactSummary` and
    // `isVisibleInTranscriptOnly` and no `isMeta` - and the wire writes
    // the whole set as one `isSynthetic`, so reading a subset here is
    // what let a summary be the reader's turn on resume and the harness's
    // line live.
    let synthetic = ["isMeta", "isCompactSummary", "isVisibleInTranscriptOnly", "turnCompanion"]
        .iter()
        .any(|flag| value.get(flag).and_then(Value::as_bool).unwrap_or(false));
    Some((
        SessionMessage {
            kind,
            uuid,
            session_id: sess,
            message: message.unwrap_or(Value::Null),
            parent_tool_use_id: None,
            timestamp,
            tool_use_result: value.get("toolUseResult").filter(|result| !result.is_null()).cloned(),
            synthetic,
        },
        boundary,
    ))
}

/// The session a transcript row belongs to. The wire spells it
/// `session_id`; a row the CLI wrote spells it `sessionId`.
fn session_in_row(value: &Value) -> String {
    value
        .get("session_id")
        .or_else(|| value.get("sessionId"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Which of the facts the kept boundary frame reads the row did not yield -
/// the trigger, and the counts either side of the cut.
///
/// Read off the same three fields `compact_boundary_frame` builds from, and
/// named rather than counted because the record built from this is the only
/// thing that would say which disk key moved.
fn missing_boundary_facts(value: &Value) -> Vec<&'static str> {
    let metadata = value.get("compactMetadata");
    let read = |key: &str| metadata.and_then(|metadata| metadata.get(key));
    [
        ("trigger", read("trigger").and_then(Value::as_str).is_none()),
        ("preTokens", read("preTokens").and_then(Value::as_u64).is_none()),
        ("postTokens", read("postTokens").and_then(Value::as_u64).is_none()),
    ]
    .into_iter()
    .filter_map(|(fact, absent)| absent.then_some(fact))
    .collect()
}

/// The boundary row in the shape the wire sends.
///
/// A transcript spells its metadata flat and camelCase (`compactMetadata`,
/// `preTokens`), while the decoder keys on the wire's nesting - so a row handed
/// on as read decodes as a generic system frame, and draws bare or half-filled
/// where a live boundary draws its counts.
fn compact_boundary_frame(value: &Value) -> Value {
    let metadata = value.get("compactMetadata");
    let field =
        |key: &str| metadata.and_then(|metadata| metadata.get(key)).cloned().unwrap_or(Value::Null);
    serde_json::json!({
        "type": "system",
        "subtype": "compact_boundary",
        "session_id": session_in_row(value),
        "uuid": value.get("uuid").and_then(Value::as_str).unwrap_or_default(),
        "compact_metadata": {
            "trigger": field("trigger"),
            "pre_tokens": field("preTokens"),
            "post_tokens": field("postTokens"),
        },
    })
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

/// True when `info` represents a worker session: one whose transcript
/// carries a `forge:worker:<label>` tag.
pub fn should_exclude_worker_tag(info: &SDKSessionInfo) -> bool {
    info.tag.as_deref().is_some_and(|t| t.starts_with(FORGE_WORKER_TAG_PREFIX))
}

/// Which worker sessions a listing may carry.
///
/// The seat decides, not the directory: a non-git worker runs in the
/// project root, so its own sessions sit in the same directory as its
/// lead's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Workers {
    /// None. A lead's listing is the project's own sessions and never
    /// its workers', and the boot catalog wants the same.
    Hidden,
    /// Only the transcripts tagged for `label`: what a worker ran under
    /// its own label is what it can resume.
    Only(String),
}

/// List sessions. When `directory` is `Some`, scans that project dir;
/// when `None`, scans every project directory under `config_dir`'s
/// `projects/` tree. Per-file lite reads run on the tokio blocking
/// pool with bounded concurrency (capped at 16 concurrent reads);
/// results are sorted by `last_modified` descending and pagination
/// applies at the end. `workers` decides which worker-tagged rows may
/// appear, and it lands BEFORE the cap: a listing is never shortened by
/// rows it would not have carried.
///
/// # Panics
///
/// Never - filesystem errors fall through and produce an empty Vec.
///
/// `tag_cache` carries the previous run's tag scans so an unchanged
/// transcript is not re-read end to end. `None` reads every byte of
/// every file, which is what a caller with no store must do.
pub async fn list_sessions(
    config_dir: &Path,
    directory: Option<&str>,
    limit: Option<usize>,
    offset: usize,
    workers: Workers,
    tag_cache: Option<&std::sync::Arc<SessionTagCache>>,
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
    let spawn_read = |set: &mut tokio::task::JoinSet<Option<SDKSessionInfo>>, path: PathBuf| {
        let cache = tag_cache.cloned();
        set.spawn_blocking(move || read_session_info(&path, cache.as_deref()));
    };
    for path in paths.by_ref().take(LIST_SESSIONS_MAX_CONCURRENT) {
        spawn_read(&mut set, path);
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
            spawn_read(&mut set, path);
        }
    }

    entries.sort_by_key(|e| std::cmp::Reverse(e.last_modified));
    let entries: Vec<SDKSessionInfo> = match &workers {
        Workers::Hidden => {
            entries.into_iter().filter(|info| !should_exclude_worker_tag(info)).collect()
        }
        Workers::Only(label) => {
            let wanted = worker_tag(label);
            entries
                .into_iter()
                .filter(|info| info.tag.as_deref() == Some(wanted.as_str()))
                .collect()
        }
    };
    let end = limit.map_or(entries.len(), |l| offset.saturating_add(l));
    entries.into_iter().skip(offset).take(end.saturating_sub(offset)).collect()
}

/// Tail shared by every `get_session_messages` bail. The compaction count
/// these paths return is unknown rather than zero, and the Projects-pane
/// row hides at zero, so "count says 0 on a session I know compacted"
/// has no triage path unless each bail is greppable.
const UNREAD_TRANSCRIPT_COUNT_NOTE: &str =
    "transcript unread, so the compaction count is unknown rather than zero";

/// Read the full transcript for one session.
///
/// Returns a default [`SessionHistory`] when the file can't be found or
/// opened - no messages and a **zero** compaction count, which is
/// indistinguishable from a session that never compacted. Each of those
/// paths logs `UNREAD_TRANSCRIPT_COUNT_NOTE` so the difference is
/// recoverable from the log even though it is not from the return value.
pub fn get_session_messages(
    config_dir: &Path,
    session_id: &str,
    directory: Option<&str>,
) -> SessionHistory {
    if !is_valid_uuid(session_id) {
        tracing::warn!(
            %session_id,
            "get_session_messages: not a session uuid; {UNREAD_TRANSCRIPT_COUNT_NOTE}",
        );
        return SessionHistory::default();
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
        tracing::debug!(
            %session_id,
            "get_session_messages: no on-disk file for session; {UNREAD_TRANSCRIPT_COUNT_NOTE}",
        );
        return SessionHistory::default();
    };
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(err) => {
            tracing::warn!(
                %session_id,
                path = %path.display(),
                %err,
                "get_session_messages: open failed; backfill renders empty and {UNREAD_TRANSCRIPT_COUNT_NOTE}",
            );
            return SessionHistory::default();
        }
    };
    parse_session_messages(file)
}

/// The rows a transcript's messages convert to, typed as the wire sends them
/// and stamped with `session_id`.
///
/// **One conversion for every read of a transcript.** A resume hands these
/// messages over as history and a paging span read hands them over as a page
/// below the window; a second copy of the conversion is how the two would
/// come to type the same row differently.
pub(crate) fn messages_from_rows(
    rows: Vec<SessionMessage>,
    session_id: &str,
) -> Vec<forge_primitives::Message> {
    let raw: Vec<Value> = rows
        .into_iter()
        .map(|m| {
            let kind = match m.kind {
                forge_primitives::SessionMessageKind::User => "user",
                forge_primitives::SessionMessageKind::Assistant => "assistant",
                forge_primitives::SessionMessageKind::System => "system",
            };
            serde_json::json!({
                "type": kind,
                "uuid": m.uuid,
                "message": m.message,
                "parent_tool_use_id": m.parent_tool_use_id,
                "timestamp": m.timestamp,
                "tool_use_result": m.tool_use_result,
                "synthetic": m.synthetic,
            })
        })
        .collect();
    let mut messages = crate::replay::synthesize_replay_messages(&raw);
    // The synthesizer leaves the session empty so the caller picks the right
    // value: the resumed session for a resume, the transcript's own for a page.
    for message in &mut messages {
        match message {
            forge_primitives::Message::Assistant { session_id: s, .. }
            | forge_primitives::Message::User { session_id: s, .. }
            | forge_primitives::Message::StopHookSummary { session_id: s, .. }
            | forge_primitives::Message::CompactBoundary { session_id: s, .. } => {
                session_id.clone_into(s);
            }
            _ => {}
        }
    }
    messages
}

/// The file a session id reads from: its project's directory when the caller
/// places it, the first copy named after it otherwise.
fn session_transcript(
    config_dir: &Path,
    session_id: &str,
    directory: Option<&str>,
) -> Option<PathBuf> {
    let file_name = format!("{session_id}.jsonl");
    if let Some(dir) = directory {
        let candidate = project_dir_for(config_dir, dir).join(&file_name);
        return candidate.is_file().then_some(candidate);
    }
    try_read_dir(&projects_dir_for(config_dir))?
        .flatten()
        .map(|entry| entry.path().join(&file_name))
        .find(|path| path.is_file())
}

/// How many bytes of a transcript one paging pass reads before growing, and
/// how far it may grow before the transcript stops answering pages.
///
/// **A page below the window's floor lies near the file's END**: the floor is
/// the oldest frame the window still holds, so the turns above it are the
/// newest rows of a file the session is still writing - some 4,000 rows, a
/// few hundred KB at the rows a transcript writes. The step is what a cold
/// session pays for its first paging request, and the cap is where the
/// transcript is treated as unreadable for paging rather than read whole.
pub const SPAN_STEP_BYTES: u64 = 1024 * 1024;
pub const SPAN_CAP_BYTES: u64 = 64 * 1024 * 1024;

/// How many rows a page's own turn count may ask for.
///
/// Sized from the turns a page is cut into: the heaviest twenty-turn run
/// measured across the eight largest transcripts on the author's machine is
/// 2,799 rows, so 150 a turn asks for 3,000 at the twenty a subscribe carries,
/// which covers every page these transcripts would cut. A page whose turns
/// run heavier comes back short with a cursor to ask below with, rather than
/// converting a window's rows for turns nothing reads.
pub const SPAN_ROWS_PER_TURN: usize = 150;

/// How many rows past a page's own count a cut may step back over, so that it
/// does not open on a result without its call.
///
/// A call and its results are adjacent rows in every transcript measured, so
/// one step is the ordinary case; the room is a short run of them - parallel
/// calls written as several result frames - and a run longer than this ends
/// the step rather than growing the page without bound.
const ROWS_PAST_CUT: usize = 8;

/// Whether a row carries a tool result: the kind of row a cut may not land on,
/// because the call it answers sits just above it in the page below.
fn carries_a_result(row: &SessionMessage) -> bool {
    row.kind == forge_primitives::SessionMessageKind::User
        && row.message.get("content").and_then(Value::as_array).is_some_and(|blocks| {
            blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        })
}

/// Read the rows of `session_id`'s transcript that sit strictly before
/// `below`, newest first, up to `rows_wanted` of them.
///
/// **Row-level, on the file's own coordinates.** A page below a window's floor
/// is a running turn's older rows, and the conversation's frame numbering
/// cannot say where they are: a window whose front is a count cut inside a
/// running turn holds frames the CLI streamed and never wrote - thinking-token
/// frames among them - so a numbering that counted those frames cannot be
/// lined up against the file's rows with arithmetic. A row's own byte needs no
/// numbering, so the read works in bytes from an anchor: the row the page
/// above handed back as its cursor, or the file's own end when the walk's
/// first ask names none.
///
/// **Bounded by construction.** The read takes a window of the file, grows it
/// only while the rows it needs are not inside the window it took, and never
/// reads past [`SPAN_CAP_BYTES`]. `None` when the transcript cannot answer for
/// this session: no file, an anchor the file does not carry, an anchor
/// further from the file's end than the cap reaches, or a window that could
/// not be read. The caller answers all of them as the end of the history,
/// which is what a page this read cannot vouch for deserves.
pub fn read_rows(
    config_dir: &Path,
    session_id: &str,
    directory: Option<&str>,
    below: Option<&forge_primitives::TranscriptAnchor>,
    rows_wanted: usize,
) -> Option<forge_primitives::TranscriptSpan> {
    read_rows_with(
        config_dir,
        session_id,
        directory,
        below,
        rows_wanted,
        SPAN_STEP_BYTES,
        SPAN_CAP_BYTES,
    )
}

/// [`read_rows`] with the page's own budget in the caller's hands, so the
/// growth, the cap and the search bound can be driven by a test without a
/// transcript of the size the production budget is written for.
fn read_rows_with(
    config_dir: &Path,
    session_id: &str,
    directory: Option<&str>,
    below: Option<&forge_primitives::TranscriptAnchor>,
    rows_wanted: usize,
    step: u64,
    cap: u64,
) -> Option<forge_primitives::TranscriptSpan> {
    // The id names the file, so anything but a uuid is a path this refuses
    // rather than resolves.
    if !is_valid_uuid(session_id) {
        return None;
    }
    let path = session_transcript(config_dir, session_id, directory)?;
    let mut file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(err) => {
            tracing::warn!(
                event_name = "span_read_open_failed",
                %session_id,
                path = %path.display(),
                %err,
                "the transcript could not be opened; the page is answered empty",
            );
            return None;
        }
    };
    let len = file.metadata().ok()?.len();
    if len == 0 {
        return None;
    }
    // **Where the read ends.** A named anchor places the window's end at its
    // own row, so the rows read are the ones strictly older than it; no
    // anchor means the file's end, whose rows are the newest the session
    // wrote - the walk's first ask on a window that holds no row to name.
    let end = match below {
        None => len,
        Some(anchor) => resolve_anchor(&mut file, len, anchor, step, cap)?,
    };

    // **The rows, walked backward from the end.** A window holds the rows just
    // below the byte it ends at; one that comes up short grows rather than
    // walking the file from its start, which on a long transcript is the whole
    // file. The windows meet on row boundaries, so no row is read twice.
    let mut kept: Vec<(SessionMessage, u64)> = Vec::new();
    let mut take = step.min(len).max(1);
    let mut at = end;
    let mut reached_start = false;
    let mut file_first: Option<u64> = None;
    loop {
        let window = match window_ending(&mut file, at, take) {
            Ok(window) => window,
            Err(err) => {
                tracing::warn!(
                    event_name = "span_read_failed",
                    %session_id,
                    from = at.saturating_sub(take),
                    %err,
                    "the transcript window could not be read; the page is answered empty",
                );
                return None;
            }
        };
        let rows: Vec<(SessionMessage, u64)> =
            window_rows(&window).into_iter().filter(|(_, offset)| *offset < at).collect();
        let next_at = rows.first().map(|(_, offset)| *offset);
        if window.from == 0 {
            reached_start = true;
            file_first = next_at;
        }
        // **The count, plus the room a cut steps over.** A cut may land among
        // result rows, and the rows it steps past are deferred to the page
        // below rather than dropped, so a little room past the count is read.
        let wanted = rows_wanted.saturating_add(ROWS_PAST_CUT);
        let room = wanted.saturating_sub(kept.len());
        let mut fresh = rows;
        if fresh.len() > room {
            // The ones nearest the boundary: a page is the newest of the older
            // rows, so what does not fit is left for the page below.
            fresh = fresh.split_off(fresh.len() - room);
        }
        let mut merged = fresh;
        merged.append(&mut kept);
        kept = merged;
        if kept.len() >= rows_wanted || window.from == 0 || take >= cap {
            break;
        }
        take = (take * 2).min(cap);
        at = next_at.unwrap_or(window.from);
        if at == 0 {
            break;
        }
    }

    // The page's own count: the room read past it was the cut's, and what the
    // cut did not need is not served.
    let over = kept.len().saturating_sub(rows_wanted);
    kept.drain(..over);
    // **A page never opens on a result without its call.** The span's oldest
    // row is where the page below stops, so a cut landing among result rows
    // would hand the fold a result whose call it does not carry - drawn with
    // its output nowhere. Those leading result rows are deferred instead: the
    // page below is cut from the call that produced them, which is older, and
    // serves them with it. Nothing is dropped - the row already read is simply
    // the page below's to carry - and a run longer than the room ends the step
    // rather than shortening the page without bound.
    let deferred = kept
        .iter()
        .position(|(row, _)| !carries_a_result(row))
        .map_or(0, |at| at.min(ROWS_PAST_CUT));
    kept.drain(..deferred);
    // The span runs to the history's start when the read reached the file's
    // own beginning AND the span's oldest row is the first row it found there:
    // a window that reached the start and held no row older than the boundary
    // has nothing left to serve either.
    let exhausted = reached_start
        && file_first.is_none_or(|first| kept.first().is_none_or(|(_, offset)| *offset == first));

    let mut messages = Vec::with_capacity(kept.len());
    let mut offsets = Vec::with_capacity(kept.len());
    for (row, offset) in kept {
        // **One row at a time**, so a row the synthesizer cannot decode is
        // dropped with its own byte rather than shifting every byte after it.
        if let Some(message) = messages_from_rows(vec![row], session_id).into_iter().next() {
            messages.push(message);
            offsets.push(offset);
        }
    }
    Some(forge_primitives::TranscriptSpan { messages, offsets, exhausted })
}

/// Where a read's window ends for a named anchor: the byte its row starts at,
/// seeking when the cursor carries a byte and searching from the file's end
/// when it does not.
///
/// `None` when the anchor cannot be placed - a row the file does not carry, or
/// one further from the end than [`SPAN_CAP_BYTES`] reaches - which the caller
/// answers as the end of the history.
fn resolve_anchor(
    file: &mut fs::File,
    len: u64,
    anchor: &forge_primitives::TranscriptAnchor,
    step: u64,
    cap: u64,
) -> Option<u64> {
    if let Some(offset) = anchor.offset.filter(|offset| *offset <= len) {
        // The verify: a resumed or rewritten transcript carries something else
        // at the byte, and the search below is what answers that.
        if anchor.row.is_empty() || row_at(file, offset).is_some_and(|row| row.uuid == anchor.row) {
            return Some(offset);
        }
    }
    if anchor.row.is_empty() {
        return None;
    }
    let mut take = step.min(len).max(1);
    loop {
        let window = window_ending(file, len, take).ok()?;
        if let Some((_, offset)) =
            window_rows(&window).into_iter().find(|(row, _)| row.uuid == anchor.row)
        {
            return Some(offset);
        }
        if take >= cap || take >= len {
            tracing::warn!(
                event_name = "span_read_anchor_unreached",
                row = %anchor.row,
                len,
                cap,
                "the anchor row is further from the file's end than the cap reaches; the page is \
                 answered empty",
            );
            return None;
        }
        take = (take * 2).min(cap);
    }
}

/// One window of a transcript file: the bytes read and the byte they start
/// at.
struct Window {
    from: u64,
    bytes: Vec<u8>,
}

/// Read up to `take` bytes ending at `end`, and the byte before the window as
/// well.
///
/// **That one byte is what says whether the window starts on a row.** A window
/// beginning exactly at a row's first byte holds that whole row; one beginning
/// anywhere else holds a fragment of the row before it, and the two are
/// indistinguishable from inside the window.
fn window_ending(file: &mut fs::File, end: u64, take: u64) -> std::io::Result<Window> {
    let from = end.saturating_sub(take).saturating_sub(1);
    let mut bytes = vec![0_u8; usize::try_from(end - from).unwrap_or(0)];
    file.seek(SeekFrom::Start(from))?;
    file.read_exact(&mut bytes)?;
    Ok(Window { from, bytes })
}

/// The rows a window holds, each with the byte it starts at.
///
/// A window whose first byte follows anything but a newline begins inside a
/// row: everything before its next newline is that row's fragment - not a row,
/// and not counted. Every other line is whole.
fn window_rows(window: &Window) -> Vec<(SessionMessage, u64)> {
    // The window's first byte is the one before the window: a newline means
    // the window begins on a row, anything else means it begins inside one.
    let fragment = window.from > 0 && window.bytes.first() != Some(&b'\n');
    let mut rows = Vec::new();
    let mut at = window.from;
    for line in window.bytes.split(|byte| *byte == b'\n') {
        let start = at;
        at = at.saturating_add(line.len() as u64 + 1);
        if fragment && start == window.from {
            continue;
        }
        if let Ok(text) = std::str::from_utf8(line)
            && let Some(row) = session_row(text)
        {
            rows.push((row, start));
        }
    }
    rows
}

/// The row that starts at `at`, for the anchored read's verify.
fn row_at(file: &mut fs::File, at: u64) -> Option<SessionMessage> {
    // One line, and a cap so a torn file cannot hand back the rest of itself.
    const LINE_CAP: u64 = 1024 * 1024;
    file.seek(SeekFrom::Start(at)).ok()?;
    let mut bytes = Vec::new();
    std::io::Read::take(&mut *file, LINE_CAP).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    session_row(text.lines().next().unwrap_or_default())
}

/// One line of a transcript as its row, or `None` for a line that is not one.
fn session_row(line: &str) -> Option<SessionMessage> {
    let line = line.trim_end_matches('\r');
    if line.is_empty() {
        return None;
    }
    let value = serde_json::from_str::<Value>(line).ok()?;
    transcript_row(&value).map(|(row, _)| row)
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
/// The tag scan that follows is NOT bounded by that buffer - it reads
/// the file end to end, so this is a lite read of the metadata and a
/// full read of the bytes. See [`scan_tag_from`].
///
/// Each `.ok()?` early return logs at debug level naming the step
/// that failed - without these, the session picker silently drops
/// sessions whose files have permission errors / are mid-truncation
/// / have a bad fd, which presents to the user as missing sessions
/// with no triage signal.
fn read_session_lite(path: &Path, cache: Option<&SessionTagCache>) -> Option<LiteSessionFile> {
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

    let tag = match tag_of(path, &mut file, size, cache) {
        Ok(tag) => tag,
        Err(e) => {
            tracing::debug!(target: crate::logging::targets::CATALOG_SCAN, path = %path.display(), error = %e, step = "seek_tag_scan", "lite-read tag-scan failed; resume will treat as untagged");
            None
        }
    };

    Some(LiteSessionFile { mtime, size, head, tail, tag })
}

/// The last tag `path`'s transcript carries, with the read error
/// [`read_session_lite`] turns into "untagged" handed back as one: a
/// caller that has to tell an unreadable transcript from an untagged one
/// cannot use the lite read.
///
/// `size` is the file's length, `file` is positioned by this call, and
/// `cache` resumes the scan where the previous one stopped - a shrunk
/// file was truncated or replaced, so the whole thing is read again.
pub(crate) fn tag_of(
    path: &Path,
    file: &mut fs::File,
    size: u64,
    cache: Option<&SessionTagCache>,
) -> std::io::Result<Option<String>> {
    let carried = cache
        .and_then(|c| c.get(path))
        .filter(|prior| prior.scanned_len <= size)
        .unwrap_or_default();
    file.seek(SeekFrom::Start(carried.scanned_len))?;
    let (tag, state) = scan_tag_from(BufReader::new(&mut *file), Some(carried));
    if let Some(cache) = cache {
        cache.put(path, state);
    }
    Ok(tag)
}

/// What a tag scan learned about one transcript, and how far into it the
/// answer is known to hold.
///
/// `scanned_len` is the offset just past the last newline the scan
/// consumed, NOT the file's length. Any trailing bytes with no newline
/// are a line the writer had not finished, so they are deliberately not
/// credited: the next scan re-reads that line from its true start. That
/// is what makes `scanned_len` a line boundary by construction, and a
/// line therefore cannot straddle it.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionTagScan {
    /// The last tag among the complete lines below `scanned_len`.
    pub tag: Option<String>,
    pub scanned_len: u64,
}

/// Carries the previous run's tag scans into this one and collects what
/// changed, so the whole catalog costs one store read up front and one
/// store write at the end rather than a transaction per transcript.
///
/// Owned by the caller because this crate has no store of its own: whoever
/// has the database fills it, passes it to [`list_sessions`], and persists
/// [`SessionTagCache::updates`] afterwards.
#[derive(Debug, Default)]
pub struct SessionTagCache {
    prior: std::collections::HashMap<String, SessionTagScan>,
    updated: std::sync::Mutex<Vec<(String, SessionTagScan)>>,
}

impl SessionTagCache {
    pub fn new(prior: std::collections::HashMap<String, SessionTagScan>) -> Self {
        Self { prior, updated: std::sync::Mutex::new(Vec::new()) }
    }

    fn get(&self, path: &Path) -> Option<SessionTagScan> {
        self.prior.get(path.to_str()?).cloned()
    }

    fn put(&self, path: &Path, scan: SessionTagScan) {
        let Some(key) = path.to_str() else { return };
        // Unchanged files re-derive the entry they already had; writing it
        // back would rewrite the whole table on every boot.
        if self.prior.get(key) == Some(&scan) {
            return;
        }
        if let Ok(mut updated) = self.updated.lock() {
            updated.push((key.to_owned(), scan));
        }
    }

    /// Entries whose scan moved this run, for the caller to persist.
    pub fn updates(&self) -> Vec<(String, SessionTagScan)> {
        self.updated.lock().map(|u| u.clone()).unwrap_or_default()
    }
}

/// Return the value of the LAST `{"type":"tag"}` row's `"tag"` field.
/// Empty strings are filtered. `None` when no tag row is present or any
/// read errors out.
///
/// Last-wins semantics: a `/new` re-tag appended later in the
/// transcript supersedes the original spawn tag.
///
/// Reads into one reused buffer rather than over `BufRead::lines`,
/// whose per-line `String` is pure overhead here: this runs over every
/// byte of every transcript in the catalog on the boot path, and the
/// tag rows it is looking for are a handful of lines in gigabytes.
///
/// `carried` is the answer a previous scan reached
/// at its own `scanned_len`, which stands unless these bytes hold a
/// later tag - last-wins makes that sound, since every line here follows
/// every line the carried answer was computed over.
///
/// Returns the tag INCLUDING any trailing unterminated line, which is
/// what a whole-file scan has always reported, alongside the state to
/// cache, which excludes it.
fn scan_tag_from<R: BufRead>(
    mut reader: R,
    carried: Option<SessionTagScan>,
) -> (Option<String>, SessionTagScan) {
    let mut state = carried.unwrap_or_default();
    // Tracked apart from `state.tag` so a tag on a half-written line is
    // reported now but not cached: the line is re-read next time.
    let mut last_tag = state.tag.clone();
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(target: crate::logging::targets::CATALOG_SCAN, error = %e, step = "tag_scan_line", "lite-read tag-scan line read failed; ending scan with last seen tag");
                break;
            }
        }
        let complete = line.last() == Some(&b'\n');
        let raw_len = line.len() as u64;
        // `lines()` yields neither terminator, and both are stripped
        // before the prefix test so a CRLF transcript compares the same
        // bytes it always did.
        if complete {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        // Every line is still validated, not only the candidates: a
        // transcript that is not UTF-8 ended the scan before, and where
        // it ends decides which tag wins. Breaking before the line is
        // credited leaves it to be retried rather than skipped for good.
        let text = match std::str::from_utf8(&line) {
            Ok(text) => text,
            Err(e) => {
                tracing::debug!(target: crate::logging::targets::CATALOG_SCAN, error = %e, step = "tag_scan_line", "lite-read tag-scan line read failed; ending scan with last seen tag");
                break;
            }
        };
        if text.starts_with("{\"type\":\"tag\"")
            && let Some(tag) = extract_last_json_string_field(text, "tag")
            && !tag.is_empty()
        {
            last_tag = Some(tag);
            if complete {
                state.tag.clone_from(&last_tag);
            }
        }
        if complete {
            state.scanned_len += raw_len;
        }
    }
    (last_tag, state)
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

fn read_session_info(path: &Path, cache: Option<&SessionTagCache>) -> Option<SDKSessionInfo> {
    let session_id = path.file_stem().and_then(|s| s.to_str())?.to_string();
    let lite = read_session_lite(path, cache)?;
    let storage_key = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned();
    let mut info = parse_session_info_from_lite(&session_id, &lite, None)?;
    info.storage_key = storage_key;
    Some(info)
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
        storage_key: String::new(),
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

    /// Buffer tracing output so an emitted record can be read back.
    #[derive(Clone, Default)]
    struct LogCapture(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture_logs(f: impl FnOnce()) -> String {
        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::fmt().with_writer(capture.clone()).finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = capture.0.lock().clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// A renamed disk field is the one drift this scan cannot otherwise see:
    /// the count stays right and the row still draws, bare or half-filled. The
    /// live arm warns on the same degradation, so the read path has to as well
    /// or the only trace of it is a row with no count and nothing saying why.
    ///
    /// **The shapes are the same failure and are keyed together**: the outer
    /// key renamed, any one of the three fields inside it renamed - the
    /// pre-count's is the one the primitives test calls plausible, and the
    /// trigger's and the post-count's are the symmetric cases - and no metadata
    /// at all. Each names what it left out, so the record points at the key
    /// that moved rather than at the row.
    #[test]
    fn a_boundary_row_whose_facts_did_not_survive_is_logged() {
        for (shape, row, missing) in [
            (
                "outer key renamed",
                r#"{"type":"system","subtype":"compact_boundary","uuid":"cb1","session_id":"s1","compactMetadataMoved":{"trigger":"auto","preTokens":41207}}"#,
                "trigger, preTokens, postTokens",
            ),
            (
                "count renamed inside it",
                r#"{"type":"system","subtype":"compact_boundary","uuid":"cb1","session_id":"s1","compactMetadata":{"trigger":"auto","pre_tokens":41207}}"#,
                "preTokens, postTokens",
            ),
            (
                "trigger renamed inside it",
                r#"{"type":"system","subtype":"compact_boundary","uuid":"cb1","session_id":"s1","compactMetadata":{"triggers":"auto","preTokens":41207}}"#,
                "trigger, postTokens",
            ),
            (
                "post count renamed inside it",
                r#"{"type":"system","subtype":"compact_boundary","uuid":"cb1","session_id":"s1","compactMetadata":{"trigger":"auto","preTokens":41207,"post_tokens":1265}}"#,
                "postTokens",
            ),
            (
                "no metadata at all",
                r#"{"type":"system","subtype":"compact_boundary","uuid":"cb1","session_id":"s1"}"#,
                "trigger, preTokens, postTokens",
            ),
        ] {
            let log = capture_logs(|| {
                parse_session_messages(row.as_bytes());
            });

            assert!(log.contains("cb1"), "the record for a {shape} names its row: {log}");
            assert!(log.contains("s1"), "and the session it was read for: {log}");
            assert!(log.contains(missing), "and the fact {shape} left out: {log}");
        }
    }

    /// The negative control: a row carrying its metadata warns about nothing,
    /// so the record above is about the drift and not about every boundary.
    #[test]
    fn a_boundary_row_with_its_metadata_logs_nothing() {
        let log = capture_logs(|| {
            parse_session_messages(
                br#"{"type":"system","subtype":"compact_boundary","uuid":"cb1","session_id":"s1","compactMetadata":{"trigger":"auto","preTokens":41207,"postTokens":1265}}"#
                    .as_slice(),
            );
        });

        assert!(log.is_empty(), "a row the scan can read warns about nothing: {log}");
    }

    #[test]
    fn sanitize_ascii_only_passthrough() {
        assert_eq!(sanitize_path("alphanum123"), "alphanum123");
    }

    #[test]
    fn sanitize_replaces_non_alphanum_with_hyphens() {
        assert_eq!(
            sanitize_path("/Users/developer/projects/forge"),
            "-Users-developer-projects-forge"
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
        assert_eq!(scan_tag_from(std::io::Cursor::new(content), None).0, None);
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
        let msgs = parse_session_messages(jsonl.as_bytes()).messages;
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

    /// The count has to come out of the same streamed pass that builds
    /// the messages. A transcript that has compacted is usually a large
    /// one (a session compacts because it is huge), so a second read of
    /// the file to count them would land squarely on the resume path for
    /// exactly the sessions this number is about.
    ///
    /// **And the row rides the list, not only the count.** A resumed
    /// conversation draws its boundaries the way a live one does, which needs
    /// the frame the fold reads: the count alone leaves a page saying
    /// "3 compactions" over a conversation with no boundary anywhere in it.
    #[test]
    fn parse_session_messages_counts_compaction_boundaries() {
        let jsonl = r#"{"type":"user","message":{"role":"user","content":"one"},"uuid":"u1","session_id":"s1"}
{"type":"system","subtype":"compact_boundary","uuid":"cb1","session_id":"s1","compactMetadata":{"trigger":"auto","preTokens":1002459,"postTokens":31253}}
{"type":"assistant","message":{"role":"assistant","content":[]},"uuid":"as1","session_id":"s1"}
{"type":"system","subtype":"compact_boundary","uuid":"cb2","session_id":"s1","compactMetadata":{"trigger":"manual","preTokens":53903,"postTokens":9149}}
{"type":"system","subtype":"other_thing","uuid":"x1","session_id":"s1"}
"#;
        let history = parse_session_messages(jsonl.as_bytes());
        assert_eq!(
            history.compaction_count, 2,
            "both boundaries counted, the other system row not"
        );
        assert_eq!(
            history.messages.len(),
            4,
            "both boundary rows ride the message list, and the unread system row does not"
        );

        // The disk row spells its metadata the CLI's way (`compactMetadata`,
        // `preTokens`) and the decoder keys on the wire's snake_case nesting,
        // so a row handed on un-normalised falls to the generic bucket and
        // draws bare where a live boundary draws its counts.
        let boundary = history
            .messages
            .iter()
            .find(|message| message.uuid == "cb1")
            .expect("the first boundary is carried");
        assert_eq!(boundary.session_id, "s1");
        assert_eq!(
            boundary.message.get("compact_metadata"),
            Some(&serde_json::json!({
                "trigger": "auto",
                "pre_tokens": 1_002_459,
                "post_tokens": 31_253,
            })),
            "normalised to the shape the wire sends",
        );
        assert_eq!(
            serde_json::from_value::<forge_primitives::Message>(boundary.message.clone())
                .expect("the kept row decodes"),
            forge_primitives::Message::CompactBoundary {
                trigger: "auto".to_owned(),
                pre_tokens: 1_002_459,
                post_tokens: 31_253,
                uuid: "cb1".to_owned(),
                session_id: "s1".to_owned(),
                metadata_extras: forge_primitives::messages::Extras::new(),
                extras: forge_primitives::messages::Extras::new(),
            },
            "and decodes typed, which is what the fold reads",
        );
    }

    #[test]
    fn parse_session_messages_counts_no_boundaries_in_a_never_compacted_session() {
        let jsonl = r#"{"type":"user","message":{"role":"user","content":"one"},"uuid":"u1","session_id":"s1"}
"#;
        assert_eq!(parse_session_messages(jsonl.as_bytes()).compaction_count, 0);
    }

    /// A turn's hooks are durable only as a `system/stop_hook_summary` row,
    /// and the scan dropped every `system` row that was not a compaction
    /// boundary, so a view opening the page fresh had nothing to draw the
    /// hook row from. The row is carried in the shape the wire sends, so the
    /// same decoder turns it into the frame the fold reads.
    #[test]
    fn parse_session_messages_keeps_a_stop_hook_summary() {
        let jsonl = r#"{"type":"user","message":{"role":"user","content":"one"},"uuid":"u1","session_id":"s1"}
{"type":"system","subtype":"stop_hook_summary","hookCount":2,"hookInfos":[{"command":"just fmt","durationMs":1400},{"command":"just check","durationMs":62000}],"hookErrors":[],"hasOutput":true,"level":"suggestion","preventedContinuation":false,"stopReason":"","toolUseID":"toolu_hook","timestamp":"2026-09-04T00:57:38.264Z","uuid":"h1","sessionId":"s1","cwd":"/proj"}
{"type":"system","subtype":"other_thing","uuid":"x1","session_id":"s1"}
"#;
        let history = parse_session_messages(jsonl.as_bytes());

        assert_eq!(history.messages.len(), 2, "the hook row joins the turn, and nothing else does");
        assert!(
            matches!(history.messages[1].kind, SessionMessageKind::System),
            "kept as the system row it is",
        );
        assert_eq!(
            history.messages[1].uuid, "h1",
            "under its own id, which is what a view keys on"
        );
        assert_eq!(history.messages[1].timestamp.as_deref(), Some("2026-09-04T00:57:38.264Z"));

        let frame: forge_primitives::Message =
            serde_json::from_value(history.messages[1].message.clone())
                .expect("the kept row decodes as the frame the wire sends");
        let forge_primitives::Message::StopHookSummary { actions, hook_infos, .. } = frame else {
            panic!("a hook summary");
        };
        assert_eq!(actions, 2, "carrying the count the row reports");
        assert_eq!(hook_infos.len(), 2, "and one entry per hook behind it");
        assert_eq!(hook_infos[0].command, "just fmt", "each naming what it ran");
        assert_eq!(hook_infos[1].duration_ms, Some(62000), "and how long it took");
    }

    #[test]
    fn parse_session_messages_skips_attachment_without_queued_command() {
        // Other attachment subtypes (image, document, etc.) are not
        // user-bubble-worthy - keep skipping them.
        let jsonl = r#"{"type":"attachment","attachment":{"type":"image","source":{}},"uuid":"a1","session_id":"s1"}
{"type":"assistant","message":{"role":"assistant","content":[]},"uuid":"as1","session_id":"s1"}
"#;
        let msgs = parse_session_messages(jsonl.as_bytes()).messages;
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
            storage_key: String::new(),
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

    /// A one-row transcript, tagged when `tag` is given.
    fn transcript(tag: Option<&str>) -> String {
        use std::fmt::Write as _;
        let mut body = "{\"type\":\"user\",\"timestamp\":\"2026-04-22T00:00:00.000Z\",\
                        \"message\":{\"content\":\"hi\"}}\n"
            .to_owned();
        if let Some(tag) = tag {
            let _ = writeln!(body, "{{\"type\":\"tag\",\"tag\":\"{tag}\"}}");
        }
        body
    }

    /// The listing's ids, in the order the scan answered them.
    fn session_ids(entries: &[SDKSessionInfo]) -> Vec<String> {
        entries.iter().map(|info| info.session_id.clone()).collect()
    }

    /// The scan's directory for `cwd`, which the listing reads.
    fn dir_for(config: &Path, cwd: &str) -> PathBuf {
        projects_dir_for(config).join(project_key_for_directory(Some(cwd)))
    }

    /// An explicit mtime, so an ordering assertion does not rest on files
    /// written inside the same millisecond.
    fn set_age(path: &Path, seconds: u64) {
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(seconds.max(1));
        fs::File::options()
            .write(true)
            .open(path)
            .expect("open for its mtime")
            .set_modified(when)
            .expect("set its mtime");
    }

    /// A listing is the seat's own sessions. A worker's label is what it
    /// can resume, and the directory cannot answer that alone: a non-git
    /// worker shares the project root with its lead, so the two seats'
    /// transcripts sit in one directory. Catches a filter that hides
    /// every worker tag whatever the seat, one that keeps another label's
    /// sessions, and one that matches a label by prefix - `alpha` must not
    /// offer `alpha-2`'s sessions, which a resume would adopt as its own.
    #[tokio::test]
    async fn a_listing_keeps_the_seats_own_sessions_and_no_others() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let cwd = cwd.to_string_lossy().into_owned();
        let dir = dir_for(tmp.path(), &cwd);
        fs::create_dir_all(&dir).unwrap();
        write_session_jsonl(&dir, "lead-own", &transcript(None));
        write_session_jsonl(&dir, "alpha-own", &transcript(Some("forge:worker:alpha")));
        write_session_jsonl(&dir, "alpha-2-own", &transcript(Some("forge:worker:alpha-2")));
        write_session_jsonl(&dir, "beta-own", &transcript(Some("forge:worker:beta")));

        let lead = list_sessions(tmp.path(), Some(&cwd), None, 0, Workers::Hidden, None).await;
        assert_eq!(
            session_ids(&lead),
            vec!["lead-own"],
            "a lead's listing is the project's own sessions, never its workers'"
        );

        let worker =
            list_sessions(tmp.path(), Some(&cwd), None, 0, Workers::Only("alpha".to_owned()), None)
                .await;
        assert_eq!(
            session_ids(&worker),
            vec!["alpha-own"],
            "a worker's listing is the sessions its own label ran, not its prefix kin's"
        );
    }

    /// The worker filter lands BEFORE the cap, so a listing is never
    /// shortened by rows it would not have carried. Catches the cap-first
    /// order: with the newest transcript a worker's, a cap of one spends
    /// its single slot on that row and answers nothing.
    #[tokio::test]
    async fn the_cap_is_applied_to_the_rows_the_listing_carries() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let cwd = cwd.to_string_lossy().into_owned();
        let dir = dir_for(tmp.path(), &cwd);
        fs::create_dir_all(&dir).unwrap();
        let held = write_session_jsonl(&dir, "lead-own", &transcript(None));
        // The worker's row is the newest, so a cap spent before the filter
        // lands on it and the listing comes back empty.
        let newest =
            write_session_jsonl(&dir, "alpha-own", &transcript(Some("forge:worker:alpha")));
        set_age(&held, 600);
        set_age(&newest, 60);

        let capped = list_sessions(tmp.path(), Some(&cwd), Some(1), 0, Workers::Hidden, None).await;
        assert_eq!(
            session_ids(&capped),
            vec!["lead-own"],
            "the cap must be spent on the rows the listing carries"
        );
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

        let info = read_session_info(&path, None).expect("session info parsed");
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

        let info = read_session_info(&path, None).expect("session info parsed");
        assert_eq!(info.tag.as_deref(), Some("forge:worker:tester"));
    }

    /// A transcript that shrank was truncated or replaced, so an offset
    /// recorded against the old bytes describes nothing in the new ones.
    /// Resuming past the end of the replacement would read no bytes at
    /// all and keep answering with a tag the file no longer contains.
    #[test]
    fn a_shrunk_transcript_is_scanned_again_rather_than_resumed() {
        let tmp = tempfile::tempdir().unwrap();
        let tagged = "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n\
                      {\"type\":\"tag\",\"tag\":\"forge:worker:gone\"}\n";
        let path = write_session_jsonl(tmp.path(), "abc", tagged);

        let cache = SessionTagCache::default();
        assert_eq!(
            read_session_lite(&path, Some(&cache)).unwrap().tag.as_deref(),
            Some("forge:worker:gone"),
            "the first scan must find the tag it will later be asked to forget"
        );

        // Replay the cache the store would have handed back, then replace
        // the file with shorter, untagged content.
        let primed = SessionTagCache::new(cache.updates().into_iter().collect());
        std::fs::write(&path, "{\"type\":\"user\",\"message\":{\"content\":\"x\"}}\n").unwrap();

        assert_eq!(
            read_session_lite(&path, Some(&primed)).unwrap().tag,
            None,
            "a replaced transcript must be re-scanned, not answered from the old offset"
        );
    }

    /// A resumed scan must agree with a whole-file one for every split
    /// point, and the hard case is a split inside a line: the writer had
    /// only flushed half a tag row when the previous scan ran. Crediting
    /// the file's length rather than the last newline would restart in
    /// the middle of that row, fail the prefix test, and lose the tag
    /// for good - it is never re-read.
    #[test]
    fn a_resumed_scan_sees_a_tag_that_was_half_written_when_it_stopped() {
        let settled = "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n";
        let tag_row = "{\"type\":\"tag\",\"tag\":\"forge-worker-x\"}\n";
        // The first scan sees a whole line plus the front of the tag row.
        let torn = &tag_row[..20];
        let (found, state) = scan_tag_from(std::io::Cursor::new(format!("{settled}{torn}")), None);
        assert_eq!(found, None, "half a tag row carries no closing quote to parse");
        assert_eq!(
            state.scanned_len,
            settled.len() as u64,
            "an unterminated line must not be credited, or it is skipped on resume"
        );

        // The writer finishes the row; the next scan resumes from the
        // recorded offset, exactly as the production path seeks.
        let whole = format!("{settled}{tag_row}");
        let resume_at = usize::try_from(state.scanned_len).unwrap();
        let (resumed, _) = scan_tag_from(std::io::Cursor::new(&whole[resume_at..]), Some(state));
        assert_eq!(
            resumed.as_deref(),
            Some("forge-worker-x"),
            "a resumed scan must find the completed row a whole-file scan would"
        );
        assert_eq!(
            resumed,
            scan_tag_from(std::io::Cursor::new(whole.as_str()), None).0,
            "resuming must agree with scanning the whole file"
        );
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

        let info = read_session_info(&path, None).expect("session info parsed");
        assert_eq!(info.tag.as_deref(), Some("second"));
    }

    #[test]
    fn read_session_info_reports_storage_dir_key() {
        // The storage key is the `projects/<KEY>/` dir the transcript
        // physically lives in - ground truth, unlike the head-read cwd.
        let tmp = tempfile::tempdir().unwrap();
        let storage_key = "-Users-me-Projects-playground--claude-worktrees-gpt-tutor";
        let project_dir = tmp.path().join(storage_key);
        fs::create_dir_all(&project_dir).unwrap();
        let path = write_session_jsonl(
            &project_dir,
            "abc",
            "{\"type\":\"user\",\"timestamp\":\"2026-04-22T00:00:00.000Z\",\"message\":{\"content\":\"hi\"}}\n",
        );

        let info = read_session_info(&path, None).expect("session info parsed");
        assert_eq!(
            info.storage_key, storage_key,
            "the scanned session reports the projects/<KEY>/ dir it lives in",
        );
    }

    /// A resumed git worker anchors its history read at the worktree run
    /// dir (`claude --resume` receives that cwd via `worker_tag_dir`), so
    /// the JSONL is read from the worktree project key, not the repo-root
    /// key. The same session id under the repo-root key is invisible -
    /// which is why the resume-map must pick a worktree-scoped session.
    #[test]
    fn get_session_messages_reads_git_worker_from_worktree_key() {
        let config_dir = tempfile::tempdir().unwrap();
        let session_id = "550e8400-e29b-41d4-a716-446655440000";
        let repo_root = "/Users/me/Projects/playground";
        let worktree = "/Users/me/Projects/playground/.claude/worktrees/gpt-tutor";

        let worktree_dir = project_dir_for(config_dir.path(), worktree);
        fs::create_dir_all(&worktree_dir).unwrap();
        write_session_jsonl(
            &worktree_dir,
            session_id,
            "{\"type\":\"user\",\"message\":{\"content\":\"hello from the worktree\"}}\n",
        );

        let from_worktree =
            get_session_messages(config_dir.path(), session_id, Some(worktree)).messages;
        assert_eq!(from_worktree.len(), 1, "resume reads the JSONL under the worktree key");

        let from_repo_root =
            get_session_messages(config_dir.path(), session_id, Some(repo_root)).messages;
        assert!(
            from_repo_root.is_empty(),
            "the same session id under the repo-root key is not where a git worker reads",
        );
    }

    /// A transcript of `rows` plain turns, each a user row carrying its own
    /// number and a uuid a caller can anchor on.
    fn a_transcript(rows: usize) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        for at in 0..rows {
            let _ = writeln!(
                out,
                "{{\"type\":\"user\",\"uuid\":\"u{at}\",\"session_id\":\"s1\",\
                 \"message\":{{\"role\":\"user\",\"content\":\"turn {at}\"}}}}"
            );
        }
        out
    }

    /// An anchor on a row the file carries, without a byte: what a caller
    /// holding the row but not its position reads by.
    fn anchored(row: &str) -> forge_primitives::TranscriptAnchor {
        forge_primitives::TranscriptAnchor { row: row.to_owned(), offset: None }
    }

    /// The words a frame carries, for a span whose rows are its turns' names.
    fn said(message: &forge_primitives::Message) -> String {
        let forge_primitives::Message::User { message, .. } = message else {
            return String::new();
        };
        message
            .content
            .iter()
            .find_map(|block| match block {
                forge_primitives::ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// The read's rows, in the words the file wrote them with.
    fn words(span: &forge_primitives::TranscriptSpan) -> Vec<String> {
        span.messages.iter().map(said).collect()
    }

    /// **A page reads the rows just below its anchor, down to the file's
    /// start.** The anchor names the row the page above stopped at; the rows
    /// the read serves are the ones strictly older than it, and the file's own
    /// first row is where the span says the history ends.
    #[test]
    fn a_row_read_serves_the_rows_below_its_anchor() {
        let config = tempfile::tempdir().unwrap();
        let cwd = "/Users/me/Projects/playground";
        let dir = project_dir_for(config.path(), cwd);
        fs::create_dir_all(&dir).unwrap();
        let session = "550e8400-e29b-41d4-a716-446655440000";
        write_session_jsonl(&dir, session, &a_transcript(40));

        let span = read_rows(config.path(), session, Some(cwd), Some(&anchored("u30")), 100)
            .expect("the transcript has rows below the anchor");

        assert!(span.exhausted, "the read reached the file's own first row");
        assert_eq!(span.messages.len(), 30, "every row below the anchor, and none above");
        assert_eq!(span.messages.len(), span.offsets.len(), "each row with the byte it starts at");
        let expected: Vec<String> = (0..30).map(|at| format!("turn {at}")).collect();
        assert_eq!(words(&span), expected, "in conversation order");

        // A page asks for its own budget of rows and gets the NEWEST of the
        // older rows: what does not fit is left for the page below it.
        let trimmed = read_rows(config.path(), session, Some(cwd), Some(&anchored("u30")), 5)
            .expect("a span the budget trims");
        assert_eq!(
            words(&trimmed),
            (25..30).map(|at| format!("turn {at}")).collect::<Vec<_>>(),
            "the page's own rows and no more, taken nearest the anchor",
        );
        assert!(!trimmed.exhausted, "which is not the file's start");

        // And the walk's first ask, on a window that holds no row to name: the
        // file's own newest rows.
        let newest = read_rows(config.path(), session, Some(cwd), None, 5)
            .expect("the file answers with its newest rows");
        assert_eq!(
            words(&newest),
            (35..40).map(|at| format!("turn {at}")).collect::<Vec<_>>(),
            "the newest rows the session wrote",
        );
        assert!(!newest.exhausted, "with more above them");
    }

    /// **A chained read seeks where a locating read searches.** Given the byte
    /// its anchor row starts at, the read takes the window ending there; given
    /// only the id, it has to find the row, which a window too small to reach
    /// it cannot do - and the walk's first page is the one that pays for the
    /// search.
    #[test]
    fn a_row_read_seeks_when_its_anchor_carries_a_byte() {
        // A cap of a couple of hundred bytes: two rows or so, which is not
        // where an anchor twenty rows back is.
        const SMALL: u64 = 200;
        let config = tempfile::tempdir().unwrap();
        let cwd = "/Users/me/Projects/playground";
        let dir = project_dir_for(config.path(), cwd);
        fs::create_dir_all(&dir).unwrap();
        let session = "550e8400-e29b-41d4-a716-446655440000";
        write_session_jsonl(&dir, session, &a_transcript(40));

        let located = read_rows(config.path(), session, Some(cwd), Some(&anchored("u30")), 100)
            .expect("the first read locates the anchor");
        let oldest = located.offsets.first().copied().expect("the span's oldest row byte");
        let oldest_row = said(located.messages.first().expect("a first"));
        assert_eq!(oldest_row, "turn 0", "precondition: the span ran to the file's first row");

        // The page below the span's own start: the next read is handed that
        // row's byte, and reads below it - which is nothing, because the span
        // held the file's start.
        let resume =
            forge_primitives::TranscriptAnchor { row: "u0".to_owned(), offset: Some(oldest) };
        let chained =
            read_rows_with(config.path(), session, Some(cwd), Some(&resume), 100, SMALL, SMALL)
                .expect("the resumed read seeks to the row's byte");
        assert!(chained.messages.is_empty(), "nothing sits below the file's first row");
        assert!(chained.exhausted, "and the read says so");

        // The same anchor without its byte cannot be reached inside the cap:
        // the search starts at the file's end and this cap does not reach
        // twenty rows back, so the read answers nothing rather than a page it
        // cannot vouch for.
        assert!(
            read_rows_with(
                config.path(),
                session,
                Some(cwd),
                Some(&anchored("u0")),
                100,
                SMALL,
                SMALL,
            )
            .is_none(),
            "a locating read whose window cannot reach its anchor answers nothing",
        );

        // And a chained read with a byte serves the rows just below it with no
        // search at all: under the same unreachably small cap, the read seeks
        // rather than searches and answers a short page with a cursor instead
        // of nothing.
        let chained = read_rows_with(
            config.path(),
            session,
            Some(cwd),
            Some(&forge_primitives::TranscriptAnchor {
                row: "u20".to_owned(),
                offset: located.offsets.get(20).copied(),
            }),
            100,
            SMALL,
            SMALL,
        )
        .expect("the chained read seeks to the row's byte");
        assert!(!chained.messages.is_empty(), "and answers the rows the byte reaches");
        assert_eq!(
            said(chained.messages.last().expect("a last")),
            "turn 19",
            "the newest row below the anchor",
        );
        assert!(!chained.exhausted, "with the file's start still above it");
    }

    /// A transcript of `pairs` tool calls, each an assistant row calling and
    /// the user row carrying its result.
    fn a_called_transcript(pairs: usize) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        for at in 0..pairs {
            let _ = writeln!(
                out,
                "{{\"type\":\"assistant\",\"uuid\":\"call{at}\",\"session_id\":\"s1\",\
                 \"message\":{{\"id\":\"m{at}\",\"role\":\"assistant\",\"model\":\"claude-opus-5\",\
                 \"content\":[{{\"type\":\"tool_use\",\
                 \"id\":\"tu{at}\",\"name\":\"Bash\",\"input\":{{\"command\":\"ls\"}}}}]}}}}"
            );
            let _ = writeln!(
                out,
                "{{\"type\":\"user\",\"uuid\":\"result{at}\",\"session_id\":\"s1\",\
                 \"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\
                 \"tool_use_id\":\"tu{at}\",\"content\":\"ok\"}}]}}}}"
            );
        }
        out
    }

    /// The ids of a span's rows, for a transcript whose rows each carry one.
    fn ids(span: &forge_primitives::TranscriptSpan) -> Vec<String> {
        span.messages
            .iter()
            .map(|message| match message {
                forge_primitives::Message::Assistant { uuid: Some(uuid), .. }
                | forge_primitives::Message::User { uuid: Some(uuid), .. }
                | forge_primitives::Message::StopHookSummary { uuid, .. }
                | forge_primitives::Message::CompactBoundary { uuid, .. } => uuid.clone(),
                other => panic!("every row of this transcript carries an id: {other:?}"),
            })
            .collect()
    }

    /// **A page never opens on a result without its call.** The rows a page
    /// keeps are the newest of the older rows, so a cut can land among result
    /// rows - and those are deferred to the page below, which is cut from the
    /// call that produced them. Nothing is dropped: the result the cut stepped
    /// past is carried with its call.
    #[test]
    fn a_row_read_defers_leading_results_to_the_page_below() {
        let config = tempfile::tempdir().unwrap();
        let cwd = "/Users/me/Projects/playground";
        let dir = project_dir_for(config.path(), cwd);
        fs::create_dir_all(&dir).unwrap();
        let session = "550e8400-e29b-41d4-a716-446655440000";
        write_session_jsonl(&dir, session, &a_called_transcript(20));

        // Three rows below the anchor land mid-pair, and the cut's room reaches
        // past that: the page opens on a CALL - the leading result rows are
        // deferred - and closes on the newest pair below the anchor.
        let span = read_rows(config.path(), session, Some(cwd), Some(&anchored("call10")), 3)
            .expect("a span below the anchor");
        assert_eq!(
            ids(&span),
            vec!["call9", "result9"],
            "the page opens on the call the cut stepped back to, rather than on a result without \
             its call",
        );

        // **The walk down, page after page.** Every page opens on a call, and
        // together they serve every row below the anchor exactly once, in
        // order - the rows a cut stepped past are the page below's newest.
        let mut seen: Vec<String> = Vec::new();
        let mut anchor = Some(anchored("call10"));
        let mut pages = 0_usize;
        while let Some(at) = anchor {
            let span = read_rows(config.path(), session, Some(cwd), Some(&at), 3).expect("a page");
            let page = ids(&span);
            assert!(
                page.first().is_some_and(|id| id.starts_with("call")),
                "every page below the floor opens on a call, never on a bare result: {page:?}",
            );
            assert!(page.len() <= 3, "and carries the rows it was asked for: {page:?}");
            seen.extend(page.clone());
            pages += 1;
            assert!(pages < 40, "the walk reaches the file's first row");
            anchor = (!span.exhausted).then(|| forge_primitives::TranscriptAnchor {
                row: page[0].clone(),
                offset: span.offsets.first().copied(),
            });
        }
        let expected: Vec<String> =
            (0..10).rev().flat_map(|at| [format!("call{at}"), format!("result{at}")]).collect();
        assert_eq!(
            seen, expected,
            "the whole span below the anchor, once each, newest page first, with every result \
             served beside the call that produced it",
        );
    }

    /// **A stale byte does not answer with a stale page.** The anchor's byte
    /// is verified before it is believed: a transcript rewritten under a walk
    /// carries another row at that byte, and the read falls back to searching
    /// for the anchor by id - so both routes answer the rows the id names.
    #[test]
    fn a_row_read_falls_back_when_the_anchors_byte_moved() {
        let config = tempfile::tempdir().unwrap();
        let cwd = "/Users/me/Projects/playground";
        let dir = project_dir_for(config.path(), cwd);
        fs::create_dir_all(&dir).unwrap();
        let session = "550e8400-e29b-41d4-a716-446655440000";
        write_session_jsonl(&dir, session, &a_transcript(40));

        let located = read_rows(config.path(), session, Some(cwd), Some(&anchored("u30")), 100)
            .expect("the first read locates the anchor");
        let last = located.messages.len() - 1;
        let resume = forge_primitives::TranscriptAnchor {
            row: "u29".to_owned(),
            offset: located.offsets.get(last).copied(),
        };
        assert!(resume.offset.is_some(), "precondition: the span gave the row its byte");

        // A row prepended: every byte below it moved, so the anchored byte no
        // longer holds the row it was read for.
        let prepended = format!(
            "{{\"type\":\"user\",\"uuid\":\"u_pre\",\"session_id\":\"s1\",\
             \"message\":{{\"role\":\"user\",\"content\":\"prepended\"}}}}\n{}",
            a_transcript(40),
        );
        write_session_jsonl(&dir, session, &prepended);

        let stale = read_rows(config.path(), session, Some(cwd), Some(&resume), 100);
        let located_again =
            read_rows(config.path(), session, Some(cwd), Some(&anchored("u29")), 100);
        assert_eq!(
            stale.map(|span| words(&span)),
            located_again.map(|span| words(&span)),
            "a stale byte answers exactly as the read that never had one: the anchor's row is \
             found again by its id, so the page below it is the page below it",
        );
    }

    /// **A sub-agent's frame has no transcript row.** The file's rule drops
    /// it, and a numbering that counted it as a row would be off for every
    /// page below it: the two halves have to say the same thing.
    #[test]
    fn a_sub_agents_frame_has_no_row() {
        let row = serde_json::json!({
            "type": "user",
            "uuid": "u1",
            "parent_tool_use_id": "toolu_dispatch",
            "session_id": "s1",
            "message": {"role": "user", "content": "under a sub-agent"},
        });
        let frame: forge_primitives::Message =
            serde_json::from_value(row.clone()).expect("a frame");
        assert!(!has_a_transcript_row(&frame), "a sub-agent's frame carries no row");
        assert!(transcript_row(&row).is_none(), "and the row rule drops it, as this says it does");
    }

    /// The one row rule's forks reach a span: a queued prompt's attachment row
    /// is hoisted into the block a view reads it as, and a boundary row is the
    /// frame it is - not skipped, and not flattened into something else.
    #[test]
    fn a_row_read_types_the_rows_the_conversion_has_forks_for() {
        let config = tempfile::tempdir().unwrap();
        let cwd = "/Users/me/Projects/playground";
        let dir = project_dir_for(config.path(), cwd);
        fs::create_dir_all(&dir).unwrap();
        let session = "550e8400-e29b-41d4-a716-446655440000";
        let body = format!(
            "{}\n{}\n{}\n{}",
            "{\"type\":\"user\",\"uuid\":\"u0\",\"session_id\":\"s1\",\
             \"message\":{\"role\":\"user\",\"content\":\"turn 0\"}}",
            "{\"type\":\"attachment\",\"uuid\":\"a1\",\"session_id\":\"s1\",\
             \"attachment\":{\"type\":\"queued_command\",\"prompt\":\"queued\",\"commandMode\":\"prompt\"}}",
            "{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"uuid\":\"h1\",\
             \"sessionId\":\"s1\",\"compactMetadata\":{\"trigger\":\"auto\",\"preTokens\":1,\"postTokens\":1}}",
            "{\"type\":\"user\",\"uuid\":\"u2\",\"session_id\":\"s1\",\
             \"message\":{\"role\":\"user\",\"content\":\"turn 1\"}}",
        );
        write_session_jsonl(&dir, session, &body);

        let span = read_rows(config.path(), session, Some(cwd), Some(&anchored("u2")), 100)
            .expect("a span over the forks");

        assert!(span.exhausted, "every row the read kept is below the anchor");
        assert_eq!(span.messages.len(), 3, "the attachment's row is a frame, not a skip");
        assert!(
            matches!(
                &span.messages[1],
                forge_primitives::Message::User { message, .. }
                    if matches!(message.content[0], forge_primitives::ContentBlock::QueuedCommand { .. })
            ),
            "the queued command arrives as the block a view reads it as",
        );
        assert!(
            matches!(&span.messages[2], forge_primitives::Message::CompactBoundary { uuid, .. }
                if uuid == "h1"),
            "and the boundary row as the frame it is, not a skip: {:?}",
            span.messages[2],
        );
    }

    /// **The read is a window, not the file.** The rows are found in a tail
    /// window, and it grows only while the rows asked for are not inside it,
    /// so the transcript's older megabytes are not read for a page that lands
    /// inside the first window.
    #[test]
    fn a_row_read_grows_its_window_only_while_the_rows_are_outside_it() {
        let config = tempfile::tempdir().unwrap();
        let cwd = "/Users/me/Projects/playground";
        let dir = project_dir_for(config.path(), cwd);
        fs::create_dir_all(&dir).unwrap();
        let session = "550e8400-e29b-41d4-a716-446655440000";
        let body = a_transcript(40);
        write_session_jsonl(&dir, session, &body);
        let len = body.len() as u64;

        // A file's worth of cap, a tenth of it a step: the first window holds
        // the newest rows and not the thirty a page asked for, so the read
        // grows - and the page it answers is the one budgeted, not the window.
        let grown = read_rows_with(config.path(), session, Some(cwd), None, 30, len / 10, len)
            .expect("the read grows to hold the page");
        assert_eq!(grown.messages.len(), 30, "the page's own rows and no more");
        assert_eq!(
            said(grown.messages.first().expect("a first")),
            "turn 10",
            "taken nearest the boundary",
        );
        assert!(!grown.exhausted, "which is not the file's start");

        // And the cap is where the search stops: an anchor further from the
        // end than the cap reaches answers nothing rather than a long read.
        assert!(
            read_rows_with(
                config.path(),
                session,
                Some(cwd),
                Some(&anchored("u5")),
                100,
                len / 5,
                len / 5,
            )
            .is_none(),
            "an anchor the cap cannot reach is not answered by reading further",
        );
    }

    /// An anchor the file does not carry is a page the read cannot vouch for:
    /// the search finds no such row and the answer is nothing, which the
    /// caller draws as the end of the history.
    #[test]
    fn a_row_read_refuses_an_anchor_the_file_does_not_carry() {
        let config = tempfile::tempdir().unwrap();
        let cwd = "/Users/me/Projects/playground";
        let dir = project_dir_for(config.path(), cwd);
        fs::create_dir_all(&dir).unwrap();
        let session = "550e8400-e29b-41d4-a716-446655440000";
        write_session_jsonl(&dir, session, &a_transcript(40));

        assert!(
            read_rows(config.path(), session, Some(cwd), Some(&anchored("nowhere")), 100).is_none(),
            "an anchor the file does not carry answers nothing",
        );
    }

    /// No file for the session is no page: the read answers nothing rather
    /// than an empty span a caller could read as the end of a history.
    #[test]
    fn a_row_read_answers_nothing_without_a_transcript() {
        let config = tempfile::tempdir().unwrap();
        let cwd = "/Users/me/Projects/playground";
        let dir = project_dir_for(config.path(), cwd);
        fs::create_dir_all(&dir).unwrap();
        let session = "550e8400-e29b-41d4-a716-446655440000";

        assert!(
            read_rows(config.path(), session, Some(cwd), Some(&anchored("u1")), 100).is_none(),
            "no file for the session is no span",
        );
        write_session_jsonl(&dir, session, "");
        assert!(
            read_rows(config.path(), session, Some(cwd), None, 100).is_none(),
            "and an empty file is none either",
        );
        assert!(
            read_rows(config.path(), "../escape", Some(cwd), None, 100).is_none(),
            "and an id that is not a uuid names a path this refuses rather than resolves",
        );
    }
}
