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

/// Skip the untracked-file inline-content scan when the working
/// tree has more than this many untracked entries. The TUI still
/// surfaces them in the file rail as `U` rows; the body just shows
/// the file list without expanded content.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    Untracked,
}

/// A single `@@`-delimited hunk inside one file's diff. `old_lines`
/// / `new_lines` carry the context lines repeated on both sides; a
/// pure-context hunk has the same vector on both fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
}

/// Top-level scanner entry. `target` is passed verbatim to `git
/// diff` as a two-dot target — `"HEAD"` for working-tree-vs-HEAD,
/// `"main"` for everything since `main` (committed + uncommitted),
/// any other ref / SHA for that comparison.
///
/// Returns one [`FileHunks`] per changed file, in the order
/// `git diff --name-status` reports them. Untracked files (when
/// `target == "HEAD"`) come at the end as
/// [`FileStatus::Untracked`].
#[must_use]
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
    // committed-since-main + uncommitted via the two-dot diff;
    // untracked files don't belong to that comparison.
    if target == "HEAD" {
        files.extend(scan_untracked(cwd).await);
    }

    files
}

/// Parse `git diff --name-status` output into a list of `FileHunks`
/// stubs. Hunks are filled in by a subsequent `merge_hunks` pass.
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
            let status = match status_code.chars().next()? {
                'M' => FileStatus::Modified,
                'A' => FileStatus::Added,
                'D' => FileStatus::Deleted,
                'R' => FileStatus::Renamed,
                'C' => FileStatus::Copied,
                _ => return None,
            };
            Some(FileHunks { path: path.to_owned(), status, hunks: Vec::new() })
        })
        .collect()
}

/// Attach hunks parsed from the full unified-diff content to each
/// file in `files` whose path matches a section in the diff. Files
/// without a matching section (binary files, etc.) keep their empty
/// hunks vector.
fn merge_hunks(files: &mut [FileHunks], diff_content: &str) {
    let sections = split_per_file(diff_content);
    for file in files.iter_mut() {
        if let Some(section) = sections.get(file.path.as_str()) {
            file.hunks = parse_hunks(section);
        }
    }
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
        let Some((old_start, old_count, new_start, new_count)) =
            parse_hunk_header(lines[start])
        else {
            continue;
        };
        let mut old_lines = Vec::new();
        let mut new_lines = Vec::new();
        for line in &lines[start + 1..end] {
            if let Some(rest) = line.strip_prefix('-') {
                old_lines.push(rest.to_owned());
            } else if let Some(rest) = line.strip_prefix('+') {
                new_lines.push(rest.to_owned());
            } else if let Some(rest) = line.strip_prefix(' ') {
                old_lines.push(rest.to_owned());
                new_lines.push(rest.to_owned());
            }
            // `\ No newline at end of file` and any stray malformed
            // lines are intentionally dropped.
        }
        hunks.push(Hunk { old_start, old_count, new_start, new_count, old_lines, new_lines });
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
                        let new_lines: Vec<String> =
                            content.lines().map(str::to_owned).collect();
                        let new_count = u32::try_from(new_lines.len()).unwrap_or(u32::MAX);
                        vec![Hunk {
                            old_start: 0,
                            old_count: 0,
                            new_start: 1,
                            new_count,
                            old_lines: Vec::new(),
                            new_lines,
                        }]
                    }
                    Err(_) => Vec::new(),
                }
            }
            _ => Vec::new(),
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
        let raw = "M\ta.rs\nA\tb.rs\nD\tc.rs\nR100\told.rs\tnew.rs\nC75\tsrc.rs\tdst.rs";
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
            ]
        );
    }

    #[test]
    fn parse_name_status_empty() {
        assert!(parse_name_status("").is_empty());
    }

    #[test]
    fn parse_hunks_single() {
        let section = "diff --git a/x.rs b/x.rs\n\
             index abc..def 100644\n\
             --- a/x.rs\n\
             +++ b/x.rs\n\
             @@ -1,3 +1,4 @@\n\
              line1\n\
              line2\n\
             +new line\n\
              line3";
        let hunks = parse_hunks(section);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[0].old_count, 3);
        assert_eq!(hunks[0].new_start, 1);
        assert_eq!(hunks[0].new_count, 4);
        assert!(hunks[0].new_lines.contains(&"new line".to_owned()));
        assert!(!hunks[0].old_lines.contains(&"new line".to_owned()));
    }

    #[test]
    fn parse_hunks_multiple() {
        let section = "diff --git a/x.rs b/x.rs\n\
             --- a/x.rs\n\
             +++ b/x.rs\n\
             @@ -1,3 +1,4 @@\n\
              line1\n\
             +added1\n\
              line2\n\
              line3\n\
             @@ -10,3 +11,4 @@\n\
              line10\n\
             +added2\n\
              line11\n\
              line12";
        let hunks = parse_hunks(section);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[1].old_start, 10);
        assert!(hunks[0].new_lines.contains(&"added1".to_owned()));
        assert!(hunks[1].new_lines.contains(&"added2".to_owned()));
    }

    #[test]
    fn parse_hunks_deletion() {
        let section = "diff --git a/x.rs b/x.rs\n\
             --- a/x.rs\n\
             +++ b/x.rs\n\
             @@ -1,4 +1,3 @@\n\
              line1\n\
             -deleted line\n\
              line2\n\
              line3";
        let hunks = parse_hunks(section);
        assert_eq!(hunks.len(), 1);
        assert!(hunks[0].old_lines.contains(&"deleted line".to_owned()));
        assert!(!hunks[0].new_lines.contains(&"deleted line".to_owned()));
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
        let raw = "diff --git a/a.rs b/a.rs\n\
             --- a/a.rs\n\
             +++ b/a.rs\n\
             @@ -1,1 +1,1 @@\n\
             -old\n\
             +new\n\
             diff --git a/b.rs b/b.rs\n\
             --- a/b.rs\n\
             +++ b/b.rs\n\
             @@ -1,1 +1,2 @@\n\
              context\n\
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
        let raw = "diff --git a/x.rs b/x.rs\n\
             --- a/x.rs\n\
             +++ b/x.rs\n\
             @@ -1,1 +1,2 @@\n\
              keep\n\
             +new";
        merge_hunks(&mut files, raw);
        assert_eq!(files[0].hunks.len(), 1);
        assert!(files[0].hunks[0].new_lines.contains(&"new".to_owned()));
    }
}
