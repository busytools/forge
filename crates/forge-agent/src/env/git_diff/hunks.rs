//! Per-file hunk scanner for the `/diff` overlay.
//!
//! Extends [`super`]'s numstat-based snapshot with full unified-diff
//! content needed to render the GitHub-style diff viewer. Runs up to
//! three subprocess calls per scan:
//!
//! 1. `git diff <target> --name-status` — file list with M/A/D/R
//!    classification.
//! 2. `git diff <target> --no-ext-diff` — full unified-diff body.
//!    The flag defeats user-configured difftastic / delta so the
//!    parser sees standard unified-diff format.
//! 3. `git ls-files --others --exclude-standard` — untracked files
//!    (only when `target == "HEAD"`, capped at [`MAX_UNTRACKED_FILES`]
//!    entries, each read up to [`MAX_UNTRACKED_FILE_SIZE`] bytes).
//!
//! Single-shot — never polled. The `/diff` overlay calls [`scan`] on
//! open and renders the resulting `Vec<FileHunks>` for the duration
//! of the view; refreshing mid-review would invalidate pending
//! comments anchored to specific lines.
//!
//! Always returns a `Vec` (possibly empty). Subprocess failures,
//! missing repos, oversize output, and timeouts all collapse to
//! empty — callers should render a "no changes" empty state on a
//! zero-length return.

use std::collections::HashMap;
use std::path::Path;

use super::{GitOutput, run_git};

/// Skip the untracked-file scan entirely when the working tree
/// has more than this many untracked entries. A fresh-repo state
/// with hundreds of untracked files would otherwise dump everything
/// into the overlay. The Inspector GIT section is unaffected; only
/// the `/diff` overlay's untracked surface is suppressed (no `U`
/// rows render in the rail when the cap is exceeded).
pub(crate) const MAX_UNTRACKED_FILES: usize = 4;

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
/// codes (`X` — internal error indicator, `B` — broken pairing)
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
    /// Unmerged — caught mid-merge-conflict. Common when the user
    /// runs `/diff` while resolving a merge.
    Unmerged,
    Untracked,
}

/// A single `@@`-delimited hunk inside one file's diff. `lines`
/// preserves the original unified-diff ordering with each line
/// tagged as context, added, or removed — the renderer iterates
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
    /// Line appears in both old and new — printed without a marker
    /// (or with a leading space, depending on the renderer).
    Context,
    /// Line is in the new file only — rendered with `+` marker.
    Added,
    /// Line is in the old file only — rendered with `-` marker.
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

/// Top-level scanner entry. `target` is passed verbatim to
/// `git diff <target>`, comparing the named ref against the working
/// tree — `"HEAD"` for working-tree-vs-HEAD (uncommitted only),
/// `"main"` for everything since `main` (committed + uncommitted),
/// any other ref / SHA for that comparison. NOT a `..` or `...`
/// range syntax; the renderer-side language ("two-dot semantics")
/// describes the result, not the input shape.
///
/// Returns one [`FileHunks`] per changed file, in the order
/// `git diff --name-status` reports them. Untracked files (when
/// `target == "HEAD"`) come at the end as
/// [`FileStatus::Untracked`].
pub async fn scan(cwd: &Path, target: &str) -> Vec<FileHunks> {
    let name_status = match run_git(cwd, &["diff", target, "--name-status"]).await {
        GitOutput::Ok(s) => s,
        GitOutput::Empty | GitOutput::Failed | GitOutput::Oversize => String::new(),
    };
    let mut files = parse_name_status(&name_status);

    if !files.is_empty() {
        let diff_content = match run_git(cwd, &["diff", target, "--no-ext-diff"]).await {
            GitOutput::Ok(s) => s,
            GitOutput::Empty | GitOutput::Failed | GitOutput::Oversize => String::new(),
        };
        merge_hunks(&mut files, &diff_content);
    }

    // Untracked only when the caller is asking for working-tree
    // semantics. `/diff main` from a feature branch already includes
    // committed-since-main + uncommitted via the ref-vs-worktree
    // compare; untracked files don't belong to that comparison.
    if target == "HEAD" {
        files.extend(scan_untracked(cwd).await);
    }

    files
}

/// Parse `git diff --name-status` output into a list of `FileHunks`
/// stubs. Hunks are filled in by a subsequent `merge_hunks` pass.
///
/// Status codes covered: M (modified), A (added), D (deleted),
/// R (renamed), C (copied), T (typechange — symlink/submodule),
/// U (unmerged — mid-merge-conflict). Unknown codes (X, B, or
/// anything else git might emit in future) fire a WARN log and
/// drop the entry so an operator can spot the gap.
fn parse_name_status(raw: &str) -> Vec<FileHunks> {
    raw.lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let status_code = parts.next()?;
            // For R/C the line is "R100\told\tnew" — the new path is
            // the last tab-separated field. For plain M/A/D it's
            // "M\tpath" — also last. `next_back` is the
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

/// Attach hunks parsed from the full unified-diff content to each
/// file in `files` whose path matches a section in the diff. Files
/// without a matching section keep their empty `hunks` vector —
/// that's the legitimate state for binary files (git emits
/// `Binary files differ` instead of `@@` hunks), submodule entries,
/// and merge-conflict entries that don't have a unified-diff
/// body. A WARN fires when a section IS matched but produces zero
/// hunks: the parser found content but couldn't extract anything,
/// which signals either a quotePath / encoding mismatch on the
/// path or a parser gap in `parse_hunks`. The renderer's "binary
/// file or no diff content" message can then be cross-referenced
/// against the log to distinguish "no body to parse" from
/// "parser missed something."
fn merge_hunks(files: &mut [FileHunks], diff_content: &str) {
    let sections = split_per_file(diff_content);
    for file in files.iter_mut() {
        let Some(section) = sections.get(file.path.as_str()) else { continue };
        let hunks = parse_hunks(section);
        if hunks.is_empty() && contains_hunk_marker(section) {
            // The section had at least one `@@` line so the parser
            // SHOULD have produced something. parse_hunks logs the
            // malformed-header case per hunk; we add this rollup
            // log so the file→section linkage failure is also
            // visible.
            tracing::warn!(
                target: crate::logging::targets::ENV_GIT,
                event_name = "git_hunks_parse_empty",
                message = "diff section had @@ markers but produced zero hunks",
                outcome = "failure",
                path = %file.path,
                section_bytes = section.len(),
            );
        }
        file.hunks = hunks;
    }
}

/// True when `section` contains at least one `@@` line — used to
/// distinguish "no body to parse" (binary / submodule, no log)
/// from "parser couldn't extract hunks despite headers being
/// present" (WARN-worthy).
fn contains_hunk_marker(section: &str) -> bool {
    section.lines().any(|line| line.starts_with("@@"))
}

/// Split the full unified-diff output into per-file sections keyed
/// by the file's path. Paths are derived from the `+++ b/<path>`
/// (or `--- a/<path>` for deletions) line inside each section
/// rather than from the `diff --git a/X b/Y` header — the header's
/// ` b/` separator is ambiguous when the path itself contains
/// ` b/` (e.g. `bokehjs/src/lib/patch.ts`).
fn split_per_file(raw: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut current_lines: Vec<&str> = Vec::new();
    let mut in_section = false;
    for line in raw.lines() {
        if line.starts_with("diff --git ") {
            if in_section {
                let path = path_from_section(&current_lines);
                if let Some(path) = path {
                    map.insert(path, current_lines.join("\n"));
                }
            }
            current_lines.clear();
            current_lines.push(line);
            in_section = true;
        } else if in_section {
            current_lines.push(line);
        }
    }
    if in_section
        && let Some(path) = path_from_section(&current_lines)
    {
        map.insert(path, current_lines.join("\n"));
    }
    map
}

/// Derive the canonical (new-side) path for a per-file diff section
/// by reading the `+++` and `---` header lines. Deletions are
/// reported back under the old path so the merger can match them
/// against the `--name-status` entry, which uses the deleted path
/// as the only path.
fn path_from_section(lines: &[&str]) -> Option<String> {
    let mut new_path: Option<String> = None;
    let mut old_path: Option<String> = None;
    let mut plus_is_devnull = false;
    for line in lines {
        if let Some(rest) = line.strip_prefix("--- a/") {
            old_path = Some(rest.to_owned());
        } else if let Some(rest) = line.strip_prefix("+++ b/") {
            new_path = Some(rest.to_owned());
            break;
        } else if line.starts_with("+++ /dev/null") {
            plus_is_devnull = true;
            break;
        }
    }
    if plus_is_devnull { old_path } else { new_path }
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
        let Some((old_start, old_count, new_start, new_count)) =
            parse_hunk_header(header_line)
        else {
            // Malformed @@ header — never expected on healthy git
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
            }
            // `\ No newline at end of file` and any stray malformed
            // lines are intentionally dropped.
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
/// hunk). Caps total count at [`MAX_UNTRACKED_FILES`] — when the
/// working tree has more untracked entries than that, returns an
/// empty `Vec` so a fresh-repo state doesn't dump hundreds of files
/// into the overlay.
async fn scan_untracked(cwd: &Path) -> Vec<FileHunks> {
    let raw = match run_git(cwd, &["ls-files", "--others", "--exclude-standard"]).await {
        GitOutput::Ok(s) => s,
        GitOutput::Empty | GitOutput::Failed | GitOutput::Oversize => return Vec::new(),
    };
    let untracked: Vec<&str> = raw.lines().filter(|l| !l.is_empty()).collect();
    if untracked.len() > MAX_UNTRACKED_FILES {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(untracked.len());
    for path in untracked {
        let file_path = cwd.join(path);
        let hunks = match tokio::fs::metadata(&file_path).await {
            Ok(meta) if meta.is_file() && meta.len() <= MAX_UNTRACKED_FILE_SIZE => {
                match tokio::fs::read_to_string(&file_path).await {
                    Ok(content) => {
                        let raw_lines: Vec<String> =
                            content.lines().map(str::to_owned).collect();
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
                // in on exotic setups. Drop them silently — the
                // rail entry remains with empty hunks.
                Vec::new()
            }
            Ok(_) => {
                // File exists but exceeds MAX_UNTRACKED_FILE_SIZE.
                // No log — this is the documented cap behavior.
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
        out.push(FileHunks {
            path: path.to_owned(),
            status: FileStatus::Untracked,
            hunks,
        });
    }
    out
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

    // Raw strings throughout — unified-diff context lines are
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
        let inserted = hunks[0]
            .lines
            .iter()
            .find(|l| added(l, "new line"))
            .expect("added line present");
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
        let context_lines: Vec<&DiffLine> = hunks[0]
            .lines
            .iter()
            .filter(|l| l.kind == DiffLineKind::Context)
            .collect();
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
        let (o_s, _, n_s, _) =
            parse_hunk_header("@@ -10,2 +10,3 @@ fn foo() {").unwrap();
        assert_eq!((o_s, n_s), (10, 10));
    }

    #[test]
    fn parse_hunk_header_malformed() {
        assert!(parse_hunk_header("not a hunk header").is_none());
        assert!(parse_hunk_header("@@ -x,y +1,2 @@").is_none());
    }

    #[test]
    fn path_from_section_addition() {
        let lines = vec!["diff --git a/new.rs b/new.rs", "--- /dev/null", "+++ b/new.rs"];
        assert_eq!(path_from_section(&lines), Some("new.rs".into()));
    }

    #[test]
    fn path_from_section_deletion() {
        let lines = vec!["diff --git a/old.rs b/old.rs", "--- a/old.rs", "+++ /dev/null"];
        assert_eq!(path_from_section(&lines), Some("old.rs".into()));
    }

    #[test]
    fn path_from_section_with_b_in_path() {
        // Regression: paths containing ` b/` used to confuse a
        // naive ` b/` split on the `diff --git` header.
        let lines = vec![
            "diff --git a/bokehjs/src/lib/patch.ts b/bokehjs/src/lib/patch.ts",
            "--- a/bokehjs/src/lib/patch.ts",
            "+++ b/bokehjs/src/lib/patch.ts",
        ];
        assert_eq!(path_from_section(&lines), Some("bokehjs/src/lib/patch.ts".into()));
    }

    #[test]
    fn split_per_file_two_files() {
        let raw = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1,1 +1,1 @@
-old
+new
diff --git a/b.rs b/b.rs
--- a/b.rs
+++ b/b.rs
@@ -1,1 +1,2 @@
 context
+added";
        let map = split_per_file(raw);
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("a.rs"));
        assert!(map.contains_key("b.rs"));
    }

    #[test]
    fn merge_hunks_attaches_to_matching_file() {
        let mut files = vec![FileHunks {
            path: "x.rs".to_owned(),
            status: FileStatus::Modified,
            hunks: Vec::new(),
        }];
        let raw = "\
diff --git a/x.rs b/x.rs
--- a/x.rs
+++ b/x.rs
@@ -1,1 +1,2 @@
 keep
+new";
        merge_hunks(&mut files, raw);
        assert_eq!(files[0].hunks.len(), 1);
        assert!(files[0].hunks[0].lines.iter().any(|l| added(l, "new")));
        assert_eq!(files[0].added_count(), 1);
        assert_eq!(files[0].removed_count(), 0);
    }
}
