//! Per-file hunk scanner for the `/diff` overlay.
//!
//! Extends [`super`]'s numstat-based snapshot with full unified-diff
//! content needed to render the GitHub-style diff viewer. Runs up to
//! three subprocess calls per scan:
//!
//! 1. `git diff <target> --name-status` - file list with M/A/D/R
//!    classification.
//! 2. `git diff <target> --no-ext-diff` - full unified-diff body.
//!    The flag defeats user-configured difftastic / delta so the
//!    parser sees standard unified-diff format.
//! 3. `git ls-files --others --exclude-standard` - untracked files
//!    (only when `target == "HEAD"`, capped at `MAX_UNTRACKED_FILES`
//!    entries, each read up to `MAX_UNTRACKED_FILE_SIZE` bytes).
//!
//! Single-shot - never polled. The `/diff` overlay calls [`scan`] on
//! open and renders the resulting `Vec<FileHunks>` for the duration
//! of the view; refreshing mid-review would invalidate pending
//! comments anchored to specific lines.
//!
//! Always returns a [`ScanOutcome`] - `files` may be empty on a
//! genuinely clean tree OR when one of the subprocess calls hit
//! Failed / Oversize. The `scanner_ok` flag distinguishes the two
//! so a downstream renderer can show "scan failed" rather than
//! miscategorising the empty result as "no changes."

use std::path::Path;

use super::{GitOutput, run_git};

/// Skip the untracked-file scan entirely when the working tree
/// has more than this many untracked entries. A fresh-repo state
/// with hundreds of untracked files would otherwise dump everything
/// into the overlay. The Inspector GIT section is unaffected; only
/// the `/diff` overlay's untracked surface is suppressed (no `U`
/// rows render in the rail when the cap is exceeded).
///
/// Public so the TUI rail renderer can surface the cap in the
/// "+N untracked suppressed (cap M)" notice - the number lives
/// here as the source of truth.
pub const MAX_UNTRACKED_FILES: usize = 4;

/// Per-file size ceiling for untracked content surfaced in the diff
/// body. Above this we keep the rail entry but skip reading bytes.
pub(crate) const MAX_UNTRACKED_FILE_SIZE: u64 = 1024;

/// One file's worth of hunks, ordered as `git diff` emitted them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHunks {
    pub path: String,
    pub status: FileStatus,
    pub hunks: Vec<Hunk>,
}

/// File-level change classification from `git diff --name-status`,
/// plus a synthetic [`Untracked`](FileStatus::Untracked) variant for
/// files surfaced from `git ls-files --others`.
///
/// Covers every status code git emits: M/A/D/R/C/T/U. Unknown
/// codes (`X` - internal error indicator, `B` - broken pairing)
/// fire a WARN log and skip the entry rather than collapsing to
/// `Modified`; legitimate user-visible types stay distinct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    /// File mode changed (regular ↔ symlink, file ↔ submodule).
    Typechange,
    /// Unmerged - caught mid-merge-conflict. Common when the user
    /// runs `/diff` while resolving a merge.
    Unmerged,
    Untracked,
}

/// A single `@@`-delimited hunk inside one file's diff. `lines`
/// preserves the original unified-diff ordering with each line
/// tagged as context, added, or removed - the renderer iterates
/// them directly to draw the `+ / - / ` markers and per-side line
/// numbers without re-classifying anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    /// Count of lines added in this hunk (`+` markers).
    pub fn added_count(&self) -> u32 {
        u32::try_from(self.lines.iter().filter(|l| l.kind == DiffLineKind::Added).count())
            .unwrap_or(u32::MAX)
    }

    /// Count of lines removed in this hunk (`-` markers).
    pub fn removed_count(&self) -> u32 {
        u32::try_from(self.lines.iter().filter(|l| l.kind == DiffLineKind::Removed).count())
            .unwrap_or(u32::MAX)
    }
}

impl FileHunks {
    /// Total additions across all hunks in this file.
    pub fn added_count(&self) -> u32 {
        self.hunks.iter().map(Hunk::added_count).fold(0u32, u32::saturating_add)
    }

    /// Total removals across all hunks in this file.
    pub fn removed_count(&self) -> u32 {
        self.hunks.iter().map(Hunk::removed_count).fold(0u32, u32::saturating_add)
    }
}

/// Per-line classification inside a hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    /// Line appears in both old and new - printed without a marker
    /// (or with a leading space, depending on the renderer).
    Context,
    /// Line is in the new file only - rendered with `+` marker.
    Added,
    /// Line is in the old file only - rendered with `-` marker.
    Removed,
}

/// One rendered diff line. Carries its kind, raw text (no leading
/// marker), and per-side line numbers so the renderer can show a
/// gutter without recomputing positions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
    /// Line number in the old file. `None` for `Added` lines.
    pub old_line: Option<u32>,
    /// Line number in the new file. `None` for `Removed` lines.
    pub new_line: Option<u32>,
}

/// Outcome of a single scan invocation. `files` carries whatever
/// was parsed; `scanner_ok` is `false` when at least one of the
/// underlying subprocess calls hit `Failed` or `Oversize` - the
/// file list may be partial or empty, and a downstream renderer
/// should distinguish that case from a genuine clean tree so the
/// user knows whether to retry vs. accept "no changes."
///
/// `untracked_suppressed` carries the count of untracked files
/// that DIDN'T make it into `files` because the working tree had
/// more than `MAX_UNTRACKED_FILES` untracked entries. The renderer
/// surfaces this so the user knows the rail is incomplete; without
/// it, a fresh-repo state with 5+ untracked files looks identical
/// to a tree with zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOutcome {
    pub files: Vec<FileHunks>,
    pub scanner_ok: bool,
    pub untracked_suppressed: usize,
}

/// Max in-flight `git diff -- <path>` subprocesses during a scan.
/// macOS's default soft open-file limit is ~256 and every git child
/// holds stdin/stdout/stderr pipes plus a handful of git internals
/// (~6-10 FDs); firing all N at once on a 100+ file refactor
/// branch trips `EMFILE`. 16 keeps total well under the FD ceiling
/// while still giving a 16× speedup over a sequential loop.
const MAX_INFLIGHT_FETCHES: usize = 16;

/// Top-level scanner entry. `target` is a plain ref / SHA / `"HEAD"`
/// (not a `..`/`...` range - the helper `diff_ref_spec` picks the
/// shape for us). `"HEAD"` runs a two-dot working-tree diff
/// (uncommitted only); any other ref runs `git diff <target>...HEAD`,
/// the three-dot form that surfaces what THIS branch contributed
/// vs the merge-base with `target`. Three-dot matches the Inspector
/// GIT panel's branch-ahead scanner (which composes
/// `format!("{default}...HEAD")` via the same helper), so the panel
/// and overlay agree on what's diffable even after a squash-merge
/// where two-dot `git diff main HEAD` would be empty.
///
/// Returns a [`ScanOutcome`] - `files` carries one [`FileHunks`]
/// per changed file in the order `git diff --name-status` reports
/// them (untracked files when `target == "HEAD"` come at the end
/// as [`FileStatus::Untracked`]), `scanner_ok` is `false` when any
/// subprocess hit `Failed`/`Oversize` so the caller can surface
/// "scan failed, check logs" rather than miscategorising the empty
/// result as "no changes."
pub async fn scan(cwd: &Path, target: &str) -> ScanOutcome {
    let mut scanner_ok = true;
    let mut untracked_suppressed: usize = 0;

    let ref_spec = diff_ref_spec(target);
    let name_status = match run_git(cwd, &["diff", &ref_spec, "--name-status"]).await {
        GitOutput::Ok(s) => s,
        GitOutput::Empty => String::new(),
        GitOutput::Failed | GitOutput::Oversize => {
            scanner_ok = false;
            String::new()
        }
    };
    let mut files = parse_name_status(&name_status);

    if !files.is_empty() {
        // Per-file concurrent fetch with bounded concurrency. Each
        // `git diff <target> --no-ext-diff -- <path>` subprocess
        // holds stdio pipes for its lifetime; firing all N
        // simultaneously via `futures::future::join_all` blows the
        // process's open-file limit on real refactor branches
        // (~130 files × ~3-10 FDs each easily exceeds the macOS
        // ~256 soft cap). `buffer_unordered(MAX_INFLIGHT_FETCHES)`
        // caps in-flight subprocesses to a safe number while still
        // giving ~MAX_INFLIGHT_FETCHES× speedup over a sequential
        // loop. Order doesn't matter because we zip back into
        // `files` by index, captured as the future's first item.
        use futures::StreamExt;
        // Clone the path list off `files` so the immutable borrow
        // drops before the stream runs - we mutate `files[idx]`
        // inside the consume loop.
        let paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
        let ref_spec_for_fetch = ref_spec.clone();
        let mut stream = futures::stream::iter(paths.into_iter().enumerate().map(|(idx, path)| {
            let ref_spec = ref_spec_for_fetch.clone();
            async move { (idx, fetch_file_hunks(cwd, &ref_spec, &path).await) }
        }))
        .buffer_unordered(MAX_INFLIGHT_FETCHES);
        while let Some((idx, result)) = stream.next().await {
            match result {
                FileScanOutcome::Ok(hunks) => files[idx].hunks = hunks,
                FileScanOutcome::SubprocessFailed => {
                    scanner_ok = false;
                }
            }
        }
    }

    // Untracked only when the caller is asking for working-tree
    // semantics. `/diff main` from a feature branch already includes
    // committed-since-main + uncommitted via the ref-vs-worktree
    // compare; untracked files don't belong to that comparison.
    if target == "HEAD" {
        let (untracked, untracked_ok, suppressed) = scan_untracked(cwd).await;
        files.extend(untracked);
        scanner_ok = scanner_ok && untracked_ok;
        untracked_suppressed = suppressed;
    }

    ScanOutcome { files, scanner_ok, untracked_suppressed }
}

/// Per-file fetch result. `SubprocessFailed` propagates back to
/// `scan` so a single-file failure flips `scanner_ok` without
/// poisoning the rest of the per-file results.
enum FileScanOutcome {
    Ok(Vec<Hunk>),
    SubprocessFailed,
}

/// Translate a caller-supplied diff `target` into the git ref-spec
/// the underlying subprocess receives.
///
/// - `HEAD` -> `HEAD` (two-dot semantics: working tree vs HEAD,
///   which is what the user means by "uncommitted edits").
/// - Anything else (branch name, SHA) -> `<target>...HEAD`
///   (three-dot semantics: diff from the merge-base of `target` and
///   `HEAD` to `HEAD`, i.e. "what did THIS branch contribute").
///
/// Why the asymmetry: after a squash-merge of the branch into the
/// target, two-dot `git diff main HEAD` is empty (content already in
/// main with a different SHA) while three-dot `git diff main...HEAD`
/// still surfaces the branch's contributions vs the merge-base. The
/// Inspector GIT panel uses three-dot via
/// `forge_agent::env::git_diff::scan`'s `format!("{default}...HEAD")`
/// at the branch-ahead layer; this helper keeps the overlay scanner
/// in sync so the panel + overlay never disagree on what's diffable.
fn diff_ref_spec(target: &str) -> String {
    if target == "HEAD" { "HEAD".to_owned() } else { format!("{target}...HEAD") }
}

/// Fetch hunks for one file via
/// `git diff <ref_spec> --no-ext-diff -- <path>`. The `--` separator
/// ensures the path isn't reinterpreted as a flag for files like
/// `--foo`. Binary files and submodule entries return `Ok(vec![])`
/// (that's the legitimate "no @@ hunks" case and matches the
/// rendering convention for those file kinds).
///
/// `ref_spec` is the already-resolved diff range from
/// [`diff_ref_spec`] (e.g. `HEAD` for working-tree diffs, or
/// `main...HEAD` for branch-against-default). Callers must pre-resolve
/// the spec so the per-file and top-level scans agree.
async fn fetch_file_hunks(cwd: &Path, ref_spec: &str, path: &str) -> FileScanOutcome {
    let section = match run_git(cwd, &["diff", ref_spec, "--no-ext-diff", "--", path]).await {
        GitOutput::Ok(s) => s,
        GitOutput::Empty => return FileScanOutcome::Ok(Vec::new()),
        GitOutput::Failed | GitOutput::Oversize => return FileScanOutcome::SubprocessFailed,
    };
    let hunks = parse_hunks(&section);
    if hunks.is_empty() && contains_hunk_marker(&section) {
        // Parser produced zero hunks despite seeing at least one
        // `@@` marker. WARN so an operator hunting "I edited this
        // file but it shows as no-diff" has a breadcrumb. The
        // per-hunk malformed-header WARN inside parse_hunks already
        // fires too; this rollup catches the section-level case.
        tracing::warn!(
            target: crate::logging::targets::ENV_GIT,
            event_name = "git_hunks_parse_empty",
            message = "diff section had @@ markers but produced zero hunks",
            outcome = "failure",
            path = %path,
            section_bytes = section.len(),
        );
    }
    FileScanOutcome::Ok(hunks)
}

/// Parse `git diff --name-status` output into a list of `FileHunks`
/// stubs. Hunks are filled in by a subsequent `merge_hunks` pass.
///
/// Status codes covered: M (modified), A (added), D (deleted),
/// R (renamed), C (copied), T (typechange - symlink/submodule),
/// U (unmerged - mid-merge-conflict). Unknown codes (X, B, or
/// anything else git might emit in future) fire a WARN log and
/// drop the entry so an operator can spot the gap.
fn parse_name_status(raw: &str) -> Vec<FileHunks> {
    raw.lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let status_code = parts.next()?;
            // For R/C the line is "R100\told\tnew" - the new path is
            // the last tab-separated field. For plain M/A/D it's
            // "M\tpath" - also last. `next_back` is the
            // DoubleEndedIterator-aware way to grab the tail without
            // walking the whole split.
            let path = parts.next_back()?;
            let leading = status_code.chars().next()?;
            let status = match leading {
                'M' => FileStatus::Modified,
                'A' => FileStatus::Added,
                'D' => FileStatus::Deleted,
                'R' => FileStatus::Renamed,
                'C' => FileStatus::Copied,
                'T' => FileStatus::Typechange,
                'U' => FileStatus::Unmerged,
                other => {
                    tracing::warn!(
                        target: crate::logging::targets::ENV_GIT,
                        event_name = "git_name_status_unknown_code",
                        message = "git diff --name-status emitted an unhandled status code; entry dropped",
                        outcome = "skipped",
                        status_code = ?other,
                        path = %path,
                    );
                    return None;
                }
            };
            Some(FileHunks { path: path.to_owned(), status, hunks: Vec::new() })
        })
        .collect()
}

/// True when `section` contains at least one `@@` line - used by
/// the per-file fetch path to distinguish "no body to parse"
/// (binary / submodule, no log) from "parser couldn't extract
/// hunks despite headers being present" (WARN-worthy).
fn contains_hunk_marker(section: &str) -> bool {
    section.lines().any(|line| line.starts_with("@@"))
}

/// Parse one file's diff section into individual hunks. Hunks are
/// `@@`-delimited; each line in a hunk body starts with one of
/// `-` (removed), `+` (added), ` ` (context), or `\` (newline
/// annotation, dropped).
fn parse_hunks(section: &str) -> Vec<Hunk> {
    let lines: Vec<&str> = section.lines().collect();
    let mut hunk_starts: Vec<usize> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("@@") {
            hunk_starts.push(i);
        }
    }
    let mut hunks = Vec::with_capacity(hunk_starts.len());
    for (idx, &start) in hunk_starts.iter().enumerate() {
        let end = hunk_starts.get(idx + 1).copied().unwrap_or(lines.len());
        let header_line = lines[start];
        let Some((old_start, old_count, new_start, new_count)) = parse_hunk_header(header_line)
        else {
            // Malformed @@ header - never expected on healthy git
            // output, but if it does fire (corrupt diff, custom
            // pretty-format leaking through) the operator needs to
            // see which line broke the parse.
            let truncated = header_line.chars().take(120).collect::<String>();
            tracing::warn!(
                target: crate::logging::targets::ENV_GIT,
                event_name = "git_hunk_header_malformed",
                message = "hunk header didn't parse; hunk dropped",
                outcome = "skipped",
                header = %truncated,
            );
            continue;
        };
        let mut diff_lines = Vec::new();
        let mut old_cursor = old_start;
        let mut new_cursor = new_start;
        for line in &lines[start + 1..end] {
            if let Some(rest) = line.strip_prefix('-') {
                diff_lines.push(DiffLine {
                    kind: DiffLineKind::Removed,
                    text: rest.to_owned(),
                    old_line: Some(old_cursor),
                    new_line: None,
                });
                old_cursor = old_cursor.saturating_add(1);
            } else if let Some(rest) = line.strip_prefix('+') {
                diff_lines.push(DiffLine {
                    kind: DiffLineKind::Added,
                    text: rest.to_owned(),
                    old_line: None,
                    new_line: Some(new_cursor),
                });
                new_cursor = new_cursor.saturating_add(1);
            } else if let Some(rest) = line.strip_prefix(' ') {
                diff_lines.push(DiffLine {
                    kind: DiffLineKind::Context,
                    text: rest.to_owned(),
                    old_line: Some(old_cursor),
                    new_line: Some(new_cursor),
                });
                old_cursor = old_cursor.saturating_add(1);
                new_cursor = new_cursor.saturating_add(1);
            } else if line.starts_with('\\') {
                // `\ No newline at end of file` - legitimate diff
                // marker, intentionally dropped.
            } else if !line.is_empty() {
                // Hunk-body line with an unexpected leading byte.
                // Healthy unified-diff output never produces this;
                // if it fires we want a breadcrumb (custom pretty-
                // format leakage, corruption, encoding mishap).
                let truncated: String = line.chars().take(120).collect();
                tracing::warn!(
                    target: crate::logging::targets::ENV_GIT,
                    event_name = "git_hunk_body_stray_line",
                    message = "hunk body line doesn't start with -/+/ /\\; line dropped",
                    outcome = "skipped",
                    line = %truncated,
                );
            }
            // Truly empty lines fall through silently - they
            // bracket the final hunk in some `git diff` outputs.
        }
        hunks.push(Hunk { old_start, old_count, new_start, new_count, lines: diff_lines });
    }
    hunks
}

/// Parse `@@ -OLD_START[,OLD_COUNT] +NEW_START[,NEW_COUNT] @@`.
/// Missing counts default to 1 per the unified-diff spec. Returns
/// `None` on malformed headers (caller skips the hunk).
fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = line.strip_prefix("@@")?;
    let payload = rest.split("@@").next()?.trim();
    let mut parts = payload.split_whitespace();
    let old_part = parts.next()?.strip_prefix('-')?;
    let new_part = parts.next()?.strip_prefix('+')?;
    let (old_start, old_count) = parse_pair(old_part)?;
    let (new_start, new_count) = parse_pair(new_part)?;
    Some((old_start, old_count, new_start, new_count))
}

fn parse_pair(s: &str) -> Option<(u32, u32)> {
    if let Some((a, b)) = s.split_once(',') {
        Some((a.parse().ok()?, b.parse().ok()?))
    } else {
        Some((s.parse().ok()?, 1))
    }
}

/// Read untracked files from `git ls-files --others
/// --exclude-standard` and synthesise a [`FileHunks`] for each one
/// (status [`Untracked`](FileStatus::Untracked), single all-added
/// hunk). Caps total count at `MAX_UNTRACKED_FILES` - when the
/// working tree has more untracked entries than that, returns an
/// empty `Vec` so a fresh-repo state doesn't dump hundreds of files
/// into the overlay.
///
/// Returns `(files, ok, suppressed)`:
/// - `ok = false` only when the underlying `ls-files` subprocess
///   hit `Failed` / `Oversize`. The cap-exceeded path keeps
///   `ok = true` because git ran fine.
/// - `suppressed` is the count of untracked files that didn't make
///   it into `files` due to `MAX_UNTRACKED_FILES`. Zero when the
///   tree was under the cap (or empty); positive when the cap
///   kicked in so the renderer can surface the truncation count.
async fn scan_untracked(cwd: &Path) -> (Vec<FileHunks>, bool, usize) {
    let raw = match run_git(cwd, &["ls-files", "--others", "--exclude-standard"]).await {
        GitOutput::Ok(s) => s,
        GitOutput::Empty => String::new(),
        GitOutput::Failed | GitOutput::Oversize => return (Vec::new(), false, 0),
    };
    let untracked: Vec<&str> = raw.lines().filter(|l| !l.is_empty()).collect();
    if untracked.len() > MAX_UNTRACKED_FILES {
        let suppressed = untracked.len();
        return (Vec::new(), true, suppressed);
    }
    let mut out = Vec::with_capacity(untracked.len());
    for path in untracked {
        let file_path = cwd.join(path);
        let hunks = match tokio::fs::metadata(&file_path).await {
            Ok(meta) if meta.is_file() && meta.len() <= MAX_UNTRACKED_FILE_SIZE => {
                match tokio::fs::read_to_string(&file_path).await {
                    Ok(content) => {
                        let raw_lines: Vec<String> = content.lines().map(str::to_owned).collect();
                        let new_count = u32::try_from(raw_lines.len()).unwrap_or(u32::MAX);
                        let diff_lines: Vec<DiffLine> = raw_lines
                            .into_iter()
                            .enumerate()
                            .map(|(i, text)| DiffLine {
                                kind: DiffLineKind::Added,
                                text,
                                old_line: None,
                                new_line: u32::try_from(i + 1).ok(),
                            })
                            .collect();
                        vec![Hunk {
                            old_start: 0,
                            old_count: 0,
                            new_start: 1,
                            new_count,
                            lines: diff_lines,
                        }]
                    }
                    Err(err) => {
                        // File showed up in `ls-files --others` and
                        // its metadata read OK, but the content
                        // read failed (permissions flipped between
                        // calls, broken symlink target, decoding
                        // error). Keep the rail entry but log so
                        // an operator can correlate "U row with no
                        // content" to the IO error.
                        tracing::warn!(
                            target: crate::logging::targets::ENV_GIT,
                            event_name = "untracked_read_failed",
                            message = "untracked file content read failed; rail entry kept with empty hunks",
                            outcome = "failure",
                            path = %file_path.display(),
                            error = %err,
                        );
                        Vec::new()
                    }
                }
            }
            Ok(meta) if !meta.is_file() => {
                // ls-files --others normally returns regular files
                // only, but submodules / sockets / fifos can sneak
                // in on exotic setups. Log so an operator hunting
                // "why is this file showing as untracked-empty"
                // has a breadcrumb - the rail row stays but its
                // body is unviewable.
                tracing::warn!(
                    target: crate::logging::targets::ENV_GIT,
                    event_name = "untracked_non_regular",
                    message = "untracked entry is not a regular file; rail entry kept with empty hunks",
                    outcome = "skipped",
                    path = %file_path.display(),
                );
                Vec::new()
            }
            Ok(meta) => {
                // File exists but exceeds MAX_UNTRACKED_FILE_SIZE.
                // Documented cap behaviour; the WARN gives an
                // operator something to grep when a specific file
                // doesn't appear with its content.
                tracing::warn!(
                    target: crate::logging::targets::ENV_GIT,
                    event_name = "untracked_oversize",
                    message = "untracked file exceeded size cap; rail entry kept with empty hunks",
                    outcome = "skipped",
                    path = %file_path.display(),
                    bytes = meta.len(),
                    cap = MAX_UNTRACKED_FILE_SIZE,
                );
                Vec::new()
            }
            Err(err) => {
                tracing::warn!(
                    target: crate::logging::targets::ENV_GIT,
                    event_name = "untracked_metadata_failed",
                    message = "untracked file metadata read failed; rail entry kept with empty hunks",
                    outcome = "failure",
                    path = %file_path.display(),
                    error = %err,
                );
                Vec::new()
            }
        };
        out.push(FileHunks { path: path.to_owned(), status: FileStatus::Untracked, hunks });
    }
    (out, true, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_name_status_modified() {
        let raw = "M\tpath/to/file.rs";
        let files = parse_name_status(raw);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "path/to/file.rs");
        assert_eq!(files[0].status, FileStatus::Modified);
    }

    #[test]
    fn parse_name_status_all_codes() {
        let raw = "\
M\ta.rs
A\tb.rs
D\tc.rs
R100\told.rs\tnew.rs
C75\tsrc.rs\tdst.rs
T\tsymlink.rs
U\tconflicted.rs";
        let files = parse_name_status(raw);
        let statuses: Vec<_> = files.iter().map(|f| (f.path.clone(), f.status)).collect();
        assert_eq!(
            statuses,
            vec![
                ("a.rs".into(), FileStatus::Modified),
                ("b.rs".into(), FileStatus::Added),
                ("c.rs".into(), FileStatus::Deleted),
                ("new.rs".into(), FileStatus::Renamed),
                ("dst.rs".into(), FileStatus::Copied),
                ("symlink.rs".into(), FileStatus::Typechange),
                ("conflicted.rs".into(), FileStatus::Unmerged),
            ]
        );
    }

    #[test]
    fn parse_name_status_drops_unknown_codes() {
        // X (internal error) and B (broken pairing) shouldn't
        // appear in normal git output but are reserved codes; the
        // parser drops them rather than miscategorising. A WARN
        // log fires per drop but tests don't capture tracing.
        let raw = "M\tkeep.rs\nX\tweird.rs\nB\tbroken.rs\nA\talso_keep.rs";
        let files = parse_name_status(raw);
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["keep.rs", "also_keep.rs"]);
    }

    #[test]
    fn parse_name_status_empty() {
        assert!(parse_name_status("").is_empty());
    }

    fn added(line: &DiffLine, expected: &str) -> bool {
        line.kind == DiffLineKind::Added && line.text == expected
    }

    fn removed(line: &DiffLine, expected: &str) -> bool {
        line.kind == DiffLineKind::Removed && line.text == expected
    }

    fn context(line: &DiffLine, expected: &str) -> bool {
        line.kind == DiffLineKind::Context && line.text == expected
    }

    // Raw strings throughout - unified-diff context lines are
    // single-space-prefixed, and Rust's `\<newline>` continuation
    // strips that space when it eats the next line's leading
    // whitespace. Raw strings preserve all whitespace verbatim.

    #[test]
    fn parse_hunks_single() {
        let section = "\
diff --git a/x.rs b/x.rs
index abc..def 100644
--- a/x.rs
+++ b/x.rs
@@ -1,3 +1,4 @@
 line1
 line2
+new line
 line3";
        let hunks = parse_hunks(section);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[0].old_count, 3);
        assert_eq!(hunks[0].new_start, 1);
        assert_eq!(hunks[0].new_count, 4);
        assert_eq!(hunks[0].added_count(), 1);
        assert_eq!(hunks[0].removed_count(), 0);
        assert!(hunks[0].lines.iter().any(|l| added(l, "new line")));
        // The added line carries its new-file line number; old_line
        // is None so a gutter renderer leaves that side blank.
        let inserted =
            hunks[0].lines.iter().find(|l| added(l, "new line")).expect("added line present");
        assert_eq!(inserted.new_line, Some(3));
        assert_eq!(inserted.old_line, None);
    }

    #[test]
    fn parse_hunks_multiple() {
        let section = "\
diff --git a/x.rs b/x.rs
--- a/x.rs
+++ b/x.rs
@@ -1,3 +1,4 @@
 line1
+added1
 line2
 line3
@@ -10,3 +11,4 @@
 line10
+added2
 line11
 line12";
        let hunks = parse_hunks(section);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[1].old_start, 10);
        assert!(hunks[0].lines.iter().any(|l| added(l, "added1")));
        assert!(hunks[1].lines.iter().any(|l| added(l, "added2")));
    }

    #[test]
    fn parse_hunks_deletion() {
        let section = "\
diff --git a/x.rs b/x.rs
--- a/x.rs
+++ b/x.rs
@@ -1,4 +1,3 @@
 line1
-deleted line
 line2
 line3";
        let hunks = parse_hunks(section);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].added_count(), 0);
        assert_eq!(hunks[0].removed_count(), 1);
        assert!(hunks[0].lines.iter().any(|l| removed(l, "deleted line")));
        let context_lines: Vec<&DiffLine> =
            hunks[0].lines.iter().filter(|l| l.kind == DiffLineKind::Context).collect();
        assert_eq!(context_lines.len(), 3);
        assert!(hunks[0].lines.iter().any(|l| context(l, "line1")));
    }

    #[test]
    fn parse_hunks_line_numbers_track_through_changes() {
        // Mixed hunk: 1 context, 1 remove, 1 add, 1 context.
        // Old:  L1 line_a, L2 line_b, L3 line_d
        // New:  L1 line_a, L2 line_c, L3 line_d
        let section = "\
diff --git a/x.rs b/x.rs
--- a/x.rs
+++ b/x.rs
@@ -1,3 +1,3 @@
 line_a
-line_b
+line_c
 line_d";
        let hunks = parse_hunks(section);
        assert_eq!(hunks.len(), 1);
        let lines = &hunks[0].lines;
        assert_eq!(lines[0].old_line, Some(1));
        assert_eq!(lines[0].new_line, Some(1));
        assert_eq!(lines[1].kind, DiffLineKind::Removed);
        assert_eq!(lines[1].old_line, Some(2));
        assert_eq!(lines[1].new_line, None);
        assert_eq!(lines[2].kind, DiffLineKind::Added);
        assert_eq!(lines[2].old_line, None);
        assert_eq!(lines[2].new_line, Some(2));
        assert_eq!(lines[3].kind, DiffLineKind::Context);
        assert_eq!(lines[3].old_line, Some(3));
        assert_eq!(lines[3].new_line, Some(3));
    }

    #[test]
    fn parse_hunk_header_with_counts() {
        let (o_s, o_c, n_s, n_c) = parse_hunk_header("@@ -65,4 +65,12 @@").unwrap();
        assert_eq!((o_s, o_c, n_s, n_c), (65, 4, 65, 12));
    }

    #[test]
    fn parse_hunk_header_omitted_counts() {
        // Per unified-diff spec, missing count means 1.
        let (o_s, o_c, n_s, n_c) = parse_hunk_header("@@ -42 +42 @@").unwrap();
        assert_eq!((o_s, o_c, n_s, n_c), (42, 1, 42, 1));
    }

    #[test]
    fn parse_hunk_header_with_trailing_context() {
        // Real git output appends the enclosing function name after
        // the second @@. Parser must ignore it.
        let (o_s, _, n_s, _) = parse_hunk_header("@@ -10,2 +10,3 @@ fn foo() {").unwrap();
        assert_eq!((o_s, n_s), (10, 10));
    }

    #[test]
    fn parse_hunk_header_malformed() {
        assert!(parse_hunk_header("not a hunk header").is_none());
        assert!(parse_hunk_header("@@ -x,y +1,2 @@").is_none());
    }

    #[test]
    fn contains_hunk_marker_true_when_at_marker_present() {
        let section = "diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n-a\n+b\n";
        assert!(contains_hunk_marker(section));
    }

    #[test]
    fn contains_hunk_marker_false_when_no_at_marker() {
        let section =
            "diff --git a/logo.png b/logo.png\nBinary files a/logo.png and b/logo.png differ\n";
        assert!(!contains_hunk_marker(section));
    }

    #[test]
    fn parse_hunks_on_full_file_section_skips_diff_headers() {
        // Regression: per-file fetch yields the full output starting
        // with `diff --git` / `index` / `--- a/` / `+++ b/` headers.
        // `parse_hunks` must skip those and only consume from the
        // first `@@` marker onward.
        let raw = "diff --git a/x.rs b/x.rs\n\
--- a/x.rs\n\
+++ b/x.rs\n\
@@ -1,1 +1,2 @@\n\
 keep\n\
+new\n";
        let hunks = parse_hunks(raw);
        assert_eq!(hunks.len(), 1);
        assert!(hunks[0].lines.iter().any(|l| added(l, "new")));
        let file = FileHunks {
            path: "x.rs".to_owned(),
            status: FileStatus::Modified,
            hunks: hunks.clone(),
        };
        assert_eq!(file.added_count(), 1);
        assert_eq!(file.removed_count(), 0);
    }

    /// `diff_ref_spec` keeps `HEAD` as a two-dot working-tree diff
    /// (otherwise `HEAD...HEAD` would be empty since HEAD is its
    /// own merge-base with itself), and converts any other target
    /// to a three-dot range. Three-dot is the same form the
    /// Inspector GIT panel uses for branch-ahead at
    /// `forge_agent::env::git_diff::scan` (`format!("{default}...HEAD")`);
    /// keeping them in sync prevents the overlay-empty / panel-says-
    /// 142-lines asymmetry that surfaces after squash-merges.
    #[test]
    fn diff_ref_spec_passes_head_through_unchanged() {
        assert_eq!(diff_ref_spec("HEAD"), "HEAD");
    }

    #[test]
    fn diff_ref_spec_converts_branch_to_three_dot_range() {
        assert_eq!(diff_ref_spec("main"), "main...HEAD");
        assert_eq!(diff_ref_spec("master"), "master...HEAD");
        assert_eq!(diff_ref_spec("origin/main"), "origin/main...HEAD");
    }

    #[test]
    fn diff_ref_spec_treats_sha_like_a_branch() {
        // Commit SHAs benefit from three-dot the same way branches
        // do - diff vs merge-base, not vs the tip directly.
        assert_eq!(diff_ref_spec("abc1234"), "abc1234...HEAD");
    }
}
