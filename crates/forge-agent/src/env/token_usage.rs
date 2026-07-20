//! Token/cost accounting for the `/usage` view.
//!
//! [`pricing`] is the LiteLLM-sourced per-model USD table used to turn
//! JSONL token counts into a notional cost. This root holds the
//! project-folding that maps a `~/.claude/projects/<slug>` directory
//! name back to the repo it belongs to.

use std::collections::{BTreeMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

pub mod pricing;

/// Encoded form of `/.claude/worktrees/` in a project slug: Claude Code
/// maps both `/` and `.` to `-`, so a worktree path folds to
/// `<parent>--claude-worktrees-<name>`.
const WORKTREE_MARKER: &str = "--claude-worktrees-";

/// Fold a `~/.claude/projects/<slug>` directory name to the display
/// project it belongs to.
///
/// The slug is the project's absolute path with `/` and `.` both
/// replaced by `-`, so it is lossy: `trader-cc` and `trader/cc` encode
/// identically. Resolution therefore consults the filesystem (the
/// user's `~/Projects`) rather than splitting on `-`. Worktrees and
/// sub-paths fold to their repo, `/tmp` paths to `scratch`.
pub fn fold_project(slug: &str) -> String {
    let projects_root = home_dir().map(|home| home.join("Projects"));
    let prefix = projects_root.as_deref().map(encoded_projects_prefix).unwrap_or_default();
    let root = projects_root.as_deref().unwrap_or_else(|| Path::new(""));
    fold_project_in(slug, &prefix, root)
}

/// Testable core of [`fold_project`]. `projects_prefix` is the encoded
/// `<home>/Projects/` string to strip; an empty prefix disables the
/// repo-resolution rule (no known home). `projects_root` is the real
/// directory the candidate repo names are stat'd against.
fn fold_project_in(slug: &str, projects_prefix: &str, projects_root: &Path) -> String {
    // (a) A worktree folds to its parent repo; the part before the
    // marker is itself the parent's slug, so recurse on it.
    if let Some(idx) = slug.find(WORKTREE_MARKER) {
        return fold_project_in(&slug[..idx], projects_prefix, projects_root);
    }
    // (b) A path under <home>/Projects/: resolve the repo name against
    // the filesystem so a dashed name (`trader-cc`) isn't mis-split.
    if !projects_prefix.is_empty()
        && let Some(remainder) = slug.strip_prefix(projects_prefix)
        && !remainder.is_empty()
    {
        return resolve_project_name(remainder, projects_root);
    }
    // (c) /tmp and /private/tmp collapse into one scratch bucket.
    if slug.starts_with("-private-tmp") || slug.starts_with("-tmp") {
        return "scratch".to_owned();
    }
    // (d) Anything else: the trailing path component.
    basename_fallback(slug)
}

/// Resolve the repo name from a slug remainder (the encoded
/// `<name>/<subpath...>` after the `Projects/` prefix). Picks the
/// longest leading run of `-`-joined tokens that is an existing
/// directory under `projects_root`; when nothing resolves (the repo
/// was removed) the first component is the best-effort label.
fn resolve_project_name(remainder: &str, projects_root: &Path) -> String {
    let tokens: Vec<&str> = remainder.split('-').collect();
    for run_len in (1..=tokens.len()).rev() {
        let candidate = tokens[..run_len].join("-");
        if projects_root.join(&candidate).is_dir() {
            return candidate;
        }
    }
    tokens.first().map_or_else(|| remainder.to_owned(), |first| (*first).to_owned())
}

/// The trailing `-`-separated component of a slug, used when no richer
/// rule applies.
fn basename_fallback(slug: &str) -> String {
    slug.rsplit('-').find(|token| !token.is_empty()).unwrap_or(slug).to_owned()
}

/// The slug prefix for paths under `<projects_root>`: the root path with
/// `/` and `.` mapped to `-`, plus the trailing separator's `-`.
fn encoded_projects_prefix(projects_root: &Path) -> String {
    format!("{}-", projects_root.to_string_lossy().replace(['/', '.'], "-"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|s| !s.is_empty()).map(PathBuf::from)
}

/// The five-way token split accumulated for one `(model, day)` bucket.
/// Cache-write is kept split by TTL tier so pricing can apply the 1h /
/// 5m rates independently.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCounts {
    pub input: u64,
    pub cache_write_1h: u64,
    pub cache_write_5m: u64,
    pub cache_read: u64,
    pub output: u64,
}

impl TokenCounts {
    fn add(&mut self, other: &TokenCounts) {
        self.input = self.input.saturating_add(other.input);
        self.cache_write_1h = self.cache_write_1h.saturating_add(other.cache_write_1h);
        self.cache_write_5m = self.cache_write_5m.saturating_add(other.cache_write_5m);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.output = self.output.saturating_add(other.output);
    }
}

/// One session file's deduped usage, keyed `model -> day -> counts`.
/// `mtime` + `size` drive the incremental cache: an unchanged file is
/// reused rather than re-parsed. `folded_project` is the repo the file
/// belongs to (see [`fold_project`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileUsageSummary {
    pub mtime: SystemTime,
    pub size: u64,
    pub folded_project: String,
    pub by_model_day: BTreeMap<String, BTreeMap<String, TokenCounts>>,
}

/// Every real session file under `projects_dir/<slug>/`. Syncthing
/// conflict copies (`*.sync-conflict-*.jsonl`) are skipped: they are
/// stale duplicates of a real session and would double-count usage that
/// per-file message-id dedup can't catch across files.
pub fn usage_files(projects_dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(project_dirs) = std::fs::read_dir(projects_dir) else {
        return files;
    };
    for project in project_dirs.flatten() {
        if !project.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(session_files) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for session in session_files.flatten() {
            let path = session.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if name.contains(".sync-conflict") {
                continue;
            }
            files.push(path);
        }
    }
    files
}

/// Parse one session file into its per-file usage summary. `None` when
/// the file can't be stat'd or opened; a malformed line is skipped.
pub fn parse_file(path: &Path) -> Option<FileUsageSummary> {
    let metadata = std::fs::metadata(path).ok()?;
    let mtime = metadata.modified().ok()?;
    let size = metadata.len();
    let slug = path.parent()?.file_name()?.to_string_lossy().into_owned();
    let folded_project = fold_project(&slug);

    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut by_model_day: BTreeMap<String, BTreeMap<String, TokenCounts>> = BTreeMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let Ok(record) = serde_json::from_str::<Record>(&line) else {
            continue;
        };
        if record.kind.as_deref() != Some("assistant") {
            continue;
        }
        let (Some(message), Some(timestamp)) = (record.message, record.timestamp) else {
            continue;
        };
        let (Some(id), Some(model), Some(usage)) = (message.id, message.model, message.usage)
        else {
            continue;
        };
        // Resumed sessions re-log prior turns into the same file; keep
        // the first occurrence of each message id.
        if !seen.insert(id) {
            continue;
        }
        let Some(day) = calendar_day(&timestamp) else {
            continue;
        };
        by_model_day.entry(model).or_default().entry(day).or_default().add(&usage.into_counts());
    }
    Some(FileUsageSummary { mtime, size, folded_project, by_model_day })
}

/// The `YYYY-MM-DD` calendar day (UTC) from an rfc3339 timestamp, or
/// `None` when the leading 10 chars aren't a valid date.
fn calendar_day(timestamp: &str) -> Option<String> {
    let day = timestamp.get(..10)?;
    let bytes = day.as_bytes();
    let ok = bytes[4] == b'-'
        && bytes[7] == b'-'
        && day[..4].bytes().all(|b| b.is_ascii_digit())
        && day[5..7].bytes().all(|b| b.is_ascii_digit())
        && day[8..10].bytes().all(|b| b.is_ascii_digit());
    ok.then(|| day.to_owned())
}

/// Minimal view of a transcript record: only the fields usage
/// accounting reads. Unknown fields (content, tool blocks, …) are
/// ignored by serde.
#[derive(Deserialize)]
struct Record {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    message: Option<RecordMessage>,
}

#[derive(Deserialize)]
struct RecordMessage {
    id: Option<String>,
    model: Option<String>,
    usage: Option<RecordUsage>,
}

#[derive(Deserialize)]
struct RecordUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_creation: Option<CacheCreation>,
}

impl RecordUsage {
    fn into_counts(self) -> TokenCounts {
        let mut counts = TokenCounts {
            input: self.input_tokens,
            cache_read: self.cache_read_input_tokens,
            output: self.output_tokens,
            ..TokenCounts::default()
        };
        // Prefer the TTL split; fall back to the flat cache-creation
        // total as the 5m tier when the record predates the split.
        match self.cache_creation {
            Some(split) => {
                counts.cache_write_1h = split.ephemeral_1h_input_tokens;
                counts.cache_write_5m = split.ephemeral_5m_input_tokens;
            }
            None => counts.cache_write_5m = self.cache_creation_input_tokens,
        }
        counts
    }
}

#[derive(Deserialize)]
struct CacheCreation {
    #[serde(default)]
    ephemeral_1h_input_tokens: u64,
    #[serde(default)]
    ephemeral_5m_input_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const PREFIX: &str = "-Users-vedhavyas-Projects-";

    fn projects_root_with(dirs: &[&str]) -> TempDir {
        let td = tempfile::tempdir().expect("tempdir");
        for dir in dirs {
            std::fs::create_dir_all(td.path().join(dir)).expect("mkdir");
        }
        td
    }

    #[test]
    fn worktree_slug_folds_to_parent_repo() {
        let root = projects_root_with(&["busymail"]);
        let slug = "-Users-vedhavyas-Projects-busymail--claude-worktrees-abuse-hardening";
        assert_eq!(fold_project_in(slug, PREFIX, root.path()), "busymail");
    }

    #[test]
    fn sub_crate_slug_folds_to_repo_not_dash_split() {
        let root = projects_root_with(&["forge"]);
        let slug = "-Users-vedhavyas-Projects-forge-crates-forge-test-harness";
        assert_eq!(fold_project_in(slug, PREFIX, root.path()), "forge");
    }

    #[test]
    fn dashed_project_name_is_not_split_on_internal_dash() {
        let root = projects_root_with(&["trader-cc"]);
        let slug = "-Users-vedhavyas-Projects-trader-cc";
        assert_eq!(fold_project_in(slug, PREFIX, root.path()), "trader-cc");
    }

    #[test]
    fn tmp_slugs_fold_to_scratch() {
        let root = projects_root_with(&[]);
        assert_eq!(
            fold_project_in("-private-tmp-forge-refresh-0ed1d9d0", PREFIX, root.path()),
            "scratch",
        );
        assert_eq!(fold_project_in("-tmp-scratchpad", PREFIX, root.path()), "scratch");
    }

    #[test]
    fn tmp_worktree_slug_folds_to_scratch_via_parent() {
        let root = projects_root_with(&[]);
        let slug = "-private-tmp-claude-501--tmpoBGeeH--claude-worktrees-harness-spawn";
        assert_eq!(fold_project_in(slug, PREFIX, root.path()), "scratch");
    }

    #[test]
    fn vanished_repo_slug_falls_back_to_first_component() {
        // The repo dir is gone, so resolution can't confirm the name;
        // the leading path component is the best-effort repo label.
        let root = projects_root_with(&[]);
        let slug = "-Users-vedhavyas-Projects-ghostrepo-src-main";
        assert_eq!(fold_project_in(slug, PREFIX, root.path()), "ghostrepo");
    }

    fn write_session(td: &TempDir, slug: &str, file: &str, lines: &[&str]) -> PathBuf {
        let dir = td.path().join(slug);
        std::fs::create_dir_all(&dir).expect("mkdir slug");
        let path = dir.join(file);
        std::fs::write(&path, lines.join("\n")).expect("write jsonl");
        path
    }

    fn day(summary: &FileUsageSummary, model: &str, day: &str) -> TokenCounts {
        summary.by_model_day.get(model).and_then(|days| days.get(day)).cloned().unwrap_or_default()
    }

    #[test]
    fn duplicate_message_id_counted_once() {
        let td = tempfile::tempdir().expect("tempdir");
        let rec = |ts: &str| {
            format!(
                r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"msg_A","model":"claude-opus-4-8","usage":{{"input_tokens":10,"output_tokens":5}}}}}}"#
            )
        };
        let path = write_session(
            &td,
            "-slug",
            "s.jsonl",
            &[&rec("2026-07-08T09:30:34.184Z"), &rec("2026-07-08T10:00:00.000Z")],
        );
        let summary = parse_file(&path).expect("parse");
        let counts = day(&summary, "claude-opus-4-8", "2026-07-08");
        assert_eq!(counts.input, 10, "the re-logged duplicate id is not added twice");
        assert_eq!(counts.output, 5);
    }

    #[test]
    fn sidechain_record_is_included() {
        let td = tempfile::tempdir().expect("tempdir");
        let line = r#"{"type":"assistant","timestamp":"2026-07-08T09:30:34.184Z","isSidechain":true,"message":{"id":"msg_S","model":"claude-opus-4-8","usage":{"input_tokens":7,"output_tokens":2}}}"#;
        let path = write_session(&td, "-slug", "s.jsonl", &[line]);
        let summary = parse_file(&path).expect("parse");
        assert_eq!(day(&summary, "claude-opus-4-8", "2026-07-08").input, 7);
    }

    #[test]
    fn day_bucket_derives_from_timestamp() {
        let td = tempfile::tempdir().expect("tempdir");
        let a = r#"{"type":"assistant","timestamp":"2026-07-08T23:59:00.000Z","message":{"id":"a","model":"m","usage":{"output_tokens":1}}}"#;
        let b = r#"{"type":"assistant","timestamp":"2026-07-09T00:01:00.000Z","message":{"id":"b","model":"m","usage":{"output_tokens":3}}}"#;
        let path = write_session(&td, "-slug", "s.jsonl", &[a, b]);
        let summary = parse_file(&path).expect("parse");
        assert_eq!(day(&summary, "m", "2026-07-08").output, 1);
        assert_eq!(day(&summary, "m", "2026-07-09").output, 3);
    }

    #[test]
    fn ephemeral_split_maps_and_flat_falls_back_to_5m() {
        let td = tempfile::tempdir().expect("tempdir");
        let split = r#"{"type":"assistant","timestamp":"2026-07-08T00:00:00Z","message":{"id":"a","model":"m","usage":{"cache_read_input_tokens":100,"cache_creation_input_tokens":23,"cache_creation":{"ephemeral_1h_input_tokens":20,"ephemeral_5m_input_tokens":3}}}}"#;
        let flat = r#"{"type":"assistant","timestamp":"2026-07-09T00:00:00Z","message":{"id":"b","model":"m","usage":{"cache_creation_input_tokens":50}}}"#;
        let path = write_session(&td, "-slug", "s.jsonl", &[split, flat]);
        let summary = parse_file(&path).expect("parse");

        let with_split = day(&summary, "m", "2026-07-08");
        assert_eq!(with_split.cache_write_1h, 20);
        assert_eq!(with_split.cache_write_5m, 3, "the split is used verbatim when present");
        assert_eq!(with_split.cache_read, 100);

        let flat_fallback = day(&summary, "m", "2026-07-09");
        assert_eq!(flat_fallback.cache_write_1h, 0);
        assert_eq!(flat_fallback.cache_write_5m, 50, "flat total falls back to the 5m tier");
    }

    #[test]
    fn usage_files_skips_sync_conflict_copies() {
        let td = tempfile::tempdir().expect("tempdir");
        write_session(&td, "-slug", "real.jsonl", &["{}"]);
        write_session(&td, "-slug", "real.sync-conflict-20260710-110136-MOGYFY5.jsonl", &["{}"]);
        let files = usage_files(td.path());
        assert_eq!(files.len(), 1, "the sync-conflict copy is excluded");
        assert!(files[0].file_name().and_then(|n| n.to_str()).is_some_and(|n| n == "real.jsonl"));
    }
}
