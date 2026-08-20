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

use forge_primitives::token_usage::{UsageReport, UsageRow, WindowUsage};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{Date, Duration, Month, OffsetDateTime};
use time_tz::{OffsetDateTimeExt, Tz};

use self::pricing::PricingTable;

pub mod pricing;

/// Encoded form of `/.claude/worktrees/` in a project slug: Claude Code
/// maps both `/` and `.` to `-`, so a worktree path folds to
/// `<parent>--claude-worktrees-<name>`.
const WORKTREE_MARKER: &str = "--claude-worktrees-";

/// Fold a `~/.claude/projects/<slug>` directory name to the display
/// project it belongs to.
///
/// The slug is the project's absolute path with `/` and `.` both
/// replaced by `-`, so it is lossy: `web-api` and `web/api` encode
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
    // the filesystem so a dashed name (`web-api`) isn't mis-split.
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
    // Drop empty tokens: a `.`/`/` in the original path encodes as a
    // dash, so a dotted or double-slashed segment leaves an empty token
    // that would otherwise resolve to `projects_root` itself (empty
    // candidate) or become an empty fallback label.
    let tokens: Vec<&str> = remainder.split('-').filter(|token| !token.is_empty()).collect();
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
    let project_dirs = match std::fs::read_dir(projects_dir) {
        Ok(dirs) => dirs,
        Err(error) => {
            tracing::warn!(
                target: "forge_agent::env::token_usage",
                %error,
                path = %projects_dir.display(),
                "reading the projects dir failed; /usage renders empty",
            );
            return files;
        }
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

/// Parse one session file into its per-file usage summary, bucketing
/// each record by its calendar day in `tz`. `None` when the file can't
/// be stat'd or opened; a malformed line is skipped.
pub fn parse_file(path: &Path, tz: &Tz) -> Option<FileUsageSummary> {
    let metadata = std::fs::metadata(path).ok()?;
    let mtime = metadata.modified().ok()?;
    let size = metadata.len();
    let slug = path.parent()?.file_name()?.to_string_lossy().into_owned();
    let folded_project = fold_project(&slug);

    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut by_model_day: BTreeMap<String, BTreeMap<String, TokenCounts>> = BTreeMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut read_error_logged = false;
    let mut timestamp_error_logged = false;
    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            // A single bad line (e.g. invalid UTF-8) must not truncate the
            // rest of the file - that would cache an undercount. Skip it
            // and warn once per file so the loss is greppable.
            Err(error) => {
                if !read_error_logged {
                    tracing::warn!(
                        target: "forge_agent::env::token_usage",
                        %error,
                        path = %path.display(),
                        "skipping an unreadable line; this file's usage may undercount",
                    );
                    read_error_logged = true;
                }
                continue;
            }
        };
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
        let Some(day) = calendar_day(&timestamp, tz) else {
            // A strict rfc3339 parse drops off-spec timestamps; warn once
            // per file so a systemic format drift is greppable rather than
            // silently rendering /usage all-zero.
            if !timestamp_error_logged {
                tracing::warn!(
                    target: "forge_agent::env::token_usage",
                    path = %path.display(),
                    "skipping a record with an unparseable timestamp; this file's usage may undercount",
                );
                timestamp_error_logged = true;
            }
            continue;
        };
        by_model_day.entry(model).or_default().entry(day).or_default().add(&usage.into_counts());
    }
    Some(FileUsageSummary { mtime, size, folded_project, by_model_day })
}

/// The `YYYY-MM-DD` calendar day in `tz` for an rfc3339 timestamp, or
/// `None` when it doesn't parse. DST-correct: the offset is resolved at
/// the instant, so a late-evening UTC time lands on the next local day.
fn calendar_day(timestamp: &str, tz: &Tz) -> Option<String> {
    let date = OffsetDateTime::parse(timestamp, &Rfc3339).ok()?.to_timezone(tz).date();
    Some(format!("{:04}-{:02}-{:02}", date.year(), u8::from(date.month()), date.day()))
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

/// Roll per-file summaries up into the four windows the `/usage`
/// overlay renders. Windows are calendar periods relative to `now`, in
/// the same local timezone the day buckets already use: today, the
/// current week (from Monday), the current month (from the 1st), and
/// all-time. Each window carries both groupings and a total row, priced
/// via `pricing` (an unpriced model contributes 0 cost).
pub fn roll_up(
    summaries: &[FileUsageSummary],
    pricing: &PricingTable,
    now: OffsetDateTime,
) -> UsageReport {
    let today = now.date();
    let week_start = today - Duration::days(i64::from(today.weekday().number_days_from_monday()));
    let month_start = Date::from_calendar_date(today.year(), today.month(), 1).unwrap_or(today);

    let mut day = WindowAcc::default();
    let mut week = WindowAcc::default();
    let mut month = WindowAcc::default();
    let mut lifetime = WindowAcc::default();

    for summary in summaries {
        for (model, days) in &summary.by_model_day {
            for (day_str, counts) in days {
                let Some(date) = parse_date(day_str) else {
                    continue;
                };
                lifetime.add(&summary.folded_project, model, counts);
                if date >= month_start {
                    month.add(&summary.folded_project, model, counts);
                }
                if date >= week_start {
                    week.add(&summary.folded_project, model, counts);
                }
                if date == today {
                    day.add(&summary.folded_project, model, counts);
                }
            }
        }
    }

    UsageReport {
        today: day.finish(pricing),
        week: week.finish(pricing),
        month: month.finish(pricing),
        lifetime: lifetime.finish(pricing),
        pricing_available: !pricing.is_empty(),
    }
}

/// Per-window accumulator keyed `project -> model -> counts`. Keeping
/// the model split under each project lets the by-project rows sum cost
/// across a project's differently-priced models.
#[derive(Default)]
struct WindowAcc {
    by_project_model: BTreeMap<String, BTreeMap<String, TokenCounts>>,
}

impl WindowAcc {
    fn add(&mut self, project: &str, model: &str, counts: &TokenCounts) {
        self.by_project_model
            .entry(project.to_owned())
            .or_default()
            .entry(model.to_owned())
            .or_default()
            .add(counts);
    }

    fn finish(&self, pricing: &PricingTable) -> WindowUsage {
        let mut model_totals: BTreeMap<String, TokenCounts> = BTreeMap::new();
        for models in self.by_project_model.values() {
            for (model, counts) in models {
                model_totals.entry(model.clone()).or_default().add(counts);
            }
        }

        let mut by_model: Vec<UsageRow> = model_totals
            .iter()
            .map(|(model, counts)| make_row(model.clone(), counts, cost_of(model, counts, pricing)))
            .collect();
        sort_by_cost_desc(&mut by_model);

        let mut by_project: Vec<UsageRow> = self
            .by_project_model
            .iter()
            .map(|(project, models)| {
                let mut counts = TokenCounts::default();
                let mut cost = 0.0;
                for (model, model_counts) in models {
                    counts.add(model_counts);
                    cost += cost_of(model, model_counts, pricing);
                }
                make_row(project.clone(), &counts, cost)
            })
            .collect();
        sort_by_cost_desc(&mut by_project);

        let mut total_counts = TokenCounts::default();
        let mut total_cost = 0.0;
        for (model, counts) in &model_totals {
            total_counts.add(counts);
            total_cost += cost_of(model, counts, pricing);
        }

        WindowUsage {
            by_model,
            by_project,
            total: make_row("TOTAL".to_owned(), &total_counts, total_cost),
        }
    }
}

fn make_row(label: String, counts: &TokenCounts, cost_usd: f64) -> UsageRow {
    UsageRow {
        label,
        input: counts.input,
        cache_write_1h: counts.cache_write_1h,
        cache_write_5m: counts.cache_write_5m,
        cache_read: counts.cache_read,
        output: counts.output,
        cost_usd,
    }
}

/// Notional cost of `counts` at `model`'s price; 0 for an unpriced or
/// `<synthetic>` model.
#[expect(
    clippy::cast_precision_loss,
    reason = "token counts stay well under 2^53, so the f64 conversion is exact"
)]
fn cost_of(model: &str, counts: &TokenCounts, pricing: &PricingTable) -> f64 {
    let Some(price) = pricing.price(model) else {
        return 0.0;
    };
    counts.input as f64 * price.input
        + counts.cache_write_1h as f64 * price.cache_write_1h
        + counts.cache_write_5m as f64 * price.cache_write_5m
        + counts.cache_read as f64 * price.cache_read
        + counts.output as f64 * price.output
}

fn sort_by_cost_desc(rows: &mut [UsageRow]) {
    rows.sort_by(|a, b| b.cost_usd.total_cmp(&a.cost_usd).then_with(|| a.label.cmp(&b.label)));
}

/// Parse a `YYYY-MM-DD` day key into a `Date`.
fn parse_date(day: &str) -> Option<Date> {
    let mut parts = day.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u8 = parts.next()?.parse().ok()?;
    let day_of_month: u8 = parts.next()?.parse().ok()?;
    Date::from_calendar_date(year, Month::try_from(month).ok()?, day_of_month).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use time_tz::timezones;

    const PREFIX: &str = "-Users-developer-Projects-";

    fn utc() -> &'static Tz {
        timezones::db::UTC
    }

    #[test]
    fn calendar_day_uses_the_injected_timezone() {
        let kolkata = timezones::get_by_name("UTC").expect("kolkata zone");
        // 2026-07-19T20:00Z is still 07-19 in UTC but 07-20 at +5:30 (01:30 local).
        assert_eq!(calendar_day("2026-07-19T20:00:00Z", utc()).as_deref(), Some("2026-07-19"));
        assert_eq!(calendar_day("2026-07-19T20:00:00Z", kolkata).as_deref(), Some("2026-07-20"));
        assert_eq!(calendar_day("not-a-timestamp", utc()), None);
    }

    fn projects_root_with(dirs: &[&str]) -> TempDir {
        let td = tempfile::tempdir().expect("tempdir");
        for dir in dirs {
            std::fs::create_dir_all(td.path().join(dir)).expect("mkdir");
        }
        td
    }

    #[test]
    fn worktree_slug_folds_to_parent_repo() {
        let root = projects_root_with(&["airmail"]);
        let slug = "-Users-developer-Projects-airmail--claude-worktrees-abuse-hardening";
        assert_eq!(fold_project_in(slug, PREFIX, root.path()), "airmail");
    }

    #[test]
    fn sub_crate_slug_folds_to_repo_not_dash_split() {
        let root = projects_root_with(&["forge"]);
        let slug = "-Users-developer-Projects-forge-crates-forge-test-harness";
        assert_eq!(fold_project_in(slug, PREFIX, root.path()), "forge");
    }

    #[test]
    fn dashed_project_name_is_not_split_on_internal_dash() {
        let root = projects_root_with(&["web-api"]);
        let slug = "-Users-developer-Projects-web-api";
        assert_eq!(fold_project_in(slug, PREFIX, root.path()), "web-api");
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
        let slug = "-Users-developer-Projects-ghostrepo-src-main";
        assert_eq!(fold_project_in(slug, PREFIX, root.path()), "ghostrepo");
    }

    #[test]
    fn dotted_path_does_not_yield_an_empty_label() {
        let root = projects_root_with(&["forge"]);
        // `~/Projects/.hidden` (dir gone) encodes the `.` as a second
        // dash; the empty candidate must not match projects_root itself.
        assert_eq!(
            fold_project_in("-Users-developer-Projects--hidden", PREFIX, root.path()),
            "hidden",
        );
        // `~/Projects/forge/.config` still folds to the repo.
        assert_eq!(
            fold_project_in("-Users-developer-Projects-forge--config", PREFIX, root.path()),
            "forge",
        );
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
        let summary = parse_file(&path, utc()).expect("parse");
        let counts = day(&summary, "claude-opus-4-8", "2026-07-08");
        assert_eq!(counts.input, 10, "the re-logged duplicate id is not added twice");
        assert_eq!(counts.output, 5);
    }

    #[test]
    fn sidechain_record_is_included() {
        let td = tempfile::tempdir().expect("tempdir");
        let line = r#"{"type":"assistant","timestamp":"2026-07-08T09:30:34.184Z","isSidechain":true,"message":{"id":"msg_S","model":"claude-opus-4-8","usage":{"input_tokens":7,"output_tokens":2}}}"#;
        let path = write_session(&td, "-slug", "s.jsonl", &[line]);
        let summary = parse_file(&path, utc()).expect("parse");
        assert_eq!(day(&summary, "claude-opus-4-8", "2026-07-08").input, 7);
    }

    #[test]
    fn day_bucket_derives_from_timestamp() {
        let td = tempfile::tempdir().expect("tempdir");
        let a = r#"{"type":"assistant","timestamp":"2026-07-08T23:59:00.000Z","message":{"id":"a","model":"m","usage":{"output_tokens":1}}}"#;
        let b = r#"{"type":"assistant","timestamp":"2026-07-09T00:01:00.000Z","message":{"id":"b","model":"m","usage":{"output_tokens":3}}}"#;
        let path = write_session(&td, "-slug", "s.jsonl", &[a, b]);
        let summary = parse_file(&path, utc()).expect("parse");
        assert_eq!(day(&summary, "m", "2026-07-08").output, 1);
        assert_eq!(day(&summary, "m", "2026-07-09").output, 3);
    }

    #[test]
    fn ephemeral_split_maps_and_flat_falls_back_to_5m() {
        let td = tempfile::tempdir().expect("tempdir");
        let split = r#"{"type":"assistant","timestamp":"2026-07-08T00:00:00Z","message":{"id":"a","model":"m","usage":{"cache_read_input_tokens":100,"cache_creation_input_tokens":23,"cache_creation":{"ephemeral_1h_input_tokens":20,"ephemeral_5m_input_tokens":3}}}}"#;
        let flat = r#"{"type":"assistant","timestamp":"2026-07-09T00:00:00Z","message":{"id":"b","model":"m","usage":{"cache_creation_input_tokens":50}}}"#;
        let path = write_session(&td, "-slug", "s.jsonl", &[split, flat]);
        let summary = parse_file(&path, utc()).expect("parse");

        let with_split = day(&summary, "m", "2026-07-08");
        assert_eq!(with_split.cache_write_1h, 20);
        assert_eq!(with_split.cache_write_5m, 3, "the split is used verbatim when present");
        assert_eq!(with_split.cache_read, 100);

        let flat_fallback = day(&summary, "m", "2026-07-09");
        assert_eq!(flat_fallback.cache_write_1h, 0);
        assert_eq!(flat_fallback.cache_write_5m, 50, "flat total falls back to the 5m tier");
    }

    #[test]
    fn parse_file_skips_an_unreadable_line_without_truncating() {
        let td = tempfile::tempdir().expect("tempdir");
        let dir = td.path().join("-slug");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let record = |id: &str, out: u8| {
            format!(
                r#"{{"type":"assistant","timestamp":"2026-07-08T00:00:00Z","message":{{"id":"{id}","model":"m","usage":{{"output_tokens":{out}}}}}}}"#
            )
            .into_bytes()
        };
        let mut bytes = record("a", 10);
        bytes.push(b'\n');
        bytes.extend_from_slice(&[0xff, 0xfe, 0xff]); // invalid UTF-8 line
        bytes.push(b'\n');
        bytes.extend_from_slice(&record("b", 5));
        let path = dir.join("s.jsonl");
        std::fs::write(&path, bytes).expect("write");

        let summary = parse_file(&path, utc()).expect("parse");
        // The unreadable middle line is skipped, not treated as EOF, so
        // the record after it still counts.
        assert_eq!(day(&summary, "m", "2026-07-08").output, 15);
    }

    #[test]
    fn parse_file_skips_a_record_with_an_unparseable_timestamp() {
        let td = tempfile::tempdir().expect("tempdir");
        let dir = td.path().join("-slug");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let rec = |id: &str, ts: &str, out: u64| {
            format!(
                r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"{id}","model":"m","usage":{{"output_tokens":{out}}}}}}}"#
            )
        };
        let path = dir.join("s.jsonl");
        std::fs::write(
            &path,
            [
                rec("a", "2026-07-08T00:00:00Z", 10),
                rec("b", "not-a-timestamp", 99),
                rec("c", "2026-07-08T01:00:00Z", 5),
            ]
            .join("\n"),
        )
        .expect("write");

        let summary = parse_file(&path, utc()).expect("parse");
        // The off-spec-timestamp record is dropped; its siblings still count.
        assert_eq!(day(&summary, "m", "2026-07-08").output, 15);
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

    fn counts_out(output: u64) -> TokenCounts {
        TokenCounts { output, ..TokenCounts::default() }
    }

    fn summary_of(project: &str, entries: &[(&str, &str, TokenCounts)]) -> FileUsageSummary {
        let mut by_model_day: BTreeMap<String, BTreeMap<String, TokenCounts>> = BTreeMap::new();
        for (model, day, counts) in entries {
            by_model_day
                .entry((*model).to_owned())
                .or_default()
                .insert((*day).to_owned(), counts.clone());
        }
        FileUsageSummary {
            mtime: SystemTime::UNIX_EPOCH,
            size: 0,
            folded_project: project.to_owned(),
            by_model_day,
        }
    }

    /// A fixed `now` mid-week (Wednesday) so `today - 1` / `today - 2`
    /// stay inside the current week regardless of the host clock.
    fn wednesday() -> OffsetDateTime {
        Date::from_calendar_date(2026, Month::July, 15).expect("valid date").midnight().assume_utc()
    }

    fn approx(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-9, "expected {expected}, got {actual}");
    }

    #[test]
    fn roll_up_buckets_days_into_windows() {
        let s = summary_of(
            "forge",
            &[
                ("m", "2026-07-15", counts_out(1)), // today
                ("m", "2026-07-14", counts_out(2)), // this week, not today
                ("m", "2026-07-05", counts_out(4)), // this month, before this week
                ("m", "2026-06-20", counts_out(8)), // last month
            ],
        );
        let report = roll_up(&[s], &PricingTable::from_litellm_json("{}"), wednesday());
        assert_eq!(report.today.total.output, 1);
        assert_eq!(report.week.total.output, 3);
        assert_eq!(report.month.total.output, 7);
        assert_eq!(report.lifetime.total.output, 15);
    }

    #[test]
    fn roll_up_flags_pricing_availability() {
        let summary = summary_of("forge", &[("m", "2026-07-15", counts_out(1))]);
        let empty = roll_up(
            std::slice::from_ref(&summary),
            &PricingTable::from_litellm_json("{}"),
            wednesday(),
        );
        assert!(!empty.pricing_available, "an empty table means pricing is unavailable");
        let priced = PricingTable::from_litellm_json(
            r#"{"m":{"input_cost_per_token":0.001,"output_cost_per_token":0.002}}"#,
        );
        assert!(roll_up(&[summary], &priced, wednesday()).pricing_available);
    }

    #[test]
    fn roll_up_prices_models_and_zeros_unpriced() {
        let s = summary_of(
            "forge",
            &[
                ("m", "2026-07-15", counts_out(1000)),
                ("<synthetic>", "2026-07-15", counts_out(500)),
            ],
        );
        let pricing = PricingTable::from_litellm_json(
            r#"{"m":{"input_cost_per_token":0.001,"output_cost_per_token":0.002}}"#,
        );
        let report = roll_up(&[s], &pricing, wednesday());
        let m = report.today.by_model.iter().find(|r| r.label == "m").expect("m row");
        let syn =
            report.today.by_model.iter().find(|r| r.label == "<synthetic>").expect("synthetic row");
        approx(m.cost_usd, 1000.0 * 0.002);
        approx(syn.cost_usd, 0.0);
        approx(report.today.total.cost_usd, 1000.0 * 0.002);
    }

    #[test]
    fn roll_up_project_cost_sums_across_models() {
        let s = summary_of(
            "forge",
            &[("cheap", "2026-07-15", counts_out(100)), ("pricey", "2026-07-15", counts_out(100))],
        );
        let pricing = PricingTable::from_litellm_json(
            r#"{"cheap":{"input_cost_per_token":0,"output_cost_per_token":0.001},
                "pricey":{"input_cost_per_token":0,"output_cost_per_token":0.01}}"#,
        );
        let report = roll_up(&[s], &pricing, wednesday());
        let forge = report.today.by_project.iter().find(|r| r.label == "forge").expect("forge row");
        assert_eq!(forge.output, 200, "project tokens sum across its models");
        approx(forge.cost_usd, 100.0 * 0.001 + 100.0 * 0.01);
    }

    #[test]
    fn roll_up_sorts_rows_by_cost_desc() {
        let s = summary_of(
            "forge",
            &[("lo", "2026-07-15", counts_out(10)), ("hi", "2026-07-15", counts_out(10))],
        );
        let pricing = PricingTable::from_litellm_json(
            r#"{"lo":{"input_cost_per_token":0,"output_cost_per_token":0.001},
                "hi":{"input_cost_per_token":0,"output_cost_per_token":0.05}}"#,
        );
        let report = roll_up(&[s], &pricing, wednesday());
        assert_eq!(report.today.by_model.first().expect("a row").label, "hi", "priciest first");
    }

    #[test]
    fn roll_up_separates_projects_and_orders_by_cost_desc() {
        let cheap = summary_of("cheap-proj", &[("m", "2026-07-15", counts_out(100))]);
        let pricey = summary_of("pricey-proj", &[("m", "2026-07-15", counts_out(1000))]);
        let pricing = PricingTable::from_litellm_json(
            r#"{"m":{"input_cost_per_token":0,"output_cost_per_token":0.01}}"#,
        );
        let report = roll_up(&[cheap, pricey], &pricing, wednesday());
        let projects = &report.lifetime.by_project;
        assert_eq!(projects.len(), 2, "two projects stay separate");
        assert_eq!(projects[0].label, "pricey-proj", "sorted by cost descending");
        assert_eq!(projects[1].label, "cheap-proj");
        assert_eq!(projects.iter().find(|r| r.label == "cheap-proj").expect("cheap").output, 100);
        assert_eq!(
            projects.iter().find(|r| r.label == "pricey-proj").expect("pricey").output,
            1000
        );
    }

    #[test]
    fn roll_up_window_edges_are_inclusive_and_keep_unpriced_tokens() {
        // now = Wed 2026-07-15 -> week_start = Mon 07-13, month_start = 07-01.
        let summary = summary_of(
            "forge",
            &[("m", "2026-07-13", counts_out(1)), ("m", "2026-07-01", counts_out(2))],
        );
        let report = roll_up(&[summary], &PricingTable::from_litellm_json("{}"), wednesday());
        assert_eq!(report.week.total.output, 1, "a record on Monday's week_start is in the week");
        assert_eq!(report.month.total.output, 3, "records on/after the 1st are in the month");
        // Empty pricing: the row keeps its full tokens, only the cost blanks.
        let row = &report.month.by_model[0];
        assert_eq!(row.output, 3, "an unpriced row keeps its full tokens");
        approx(row.cost_usd, 0.0);
    }
}
