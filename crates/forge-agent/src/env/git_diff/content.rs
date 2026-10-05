//! Bounded content read beside the stats scan: what a seat's changed
//! files say, as raw unified-diff hunks.
//!
//! The sibling of [`super::scan`], not a layer inside it: the stats scan
//! is polled (1 Hz on a moving tree, 10 s when still) and its `prev` is
//! the PR-lookup cache, while this read is taken for a seat's record on
//! the reads that encode one - a cold load, a reconnect, a seat swap -
//! and never polled. The `/diff` overlay's open-time scan is the same
//! bargain on the terminal side.
//!
//! One parser for both readers: the hunks and statuses are
//! [`super::hunks`]' own, and only the caps below are this read's. A
//! record is re-encoded per read and re-sent on reconnect, so no read
//! may carry an unbounded diff, and every cap has a flag beside it
//! rather than a silence.
//!
//! Both layers the snapshot carries are read with the refs the stats
//! scan used - `HEAD` for the worktree layer, `<default>...HEAD` for the
//! branch-ahead one - so a file's row and its content describe one tree.

use std::path::Path;

use serde::{Deserialize, Serialize};

use forge_primitives::git_diff::{GitDiffSnapshot, LayerState};

use super::hunks::{
    FileStatus, Hunk, MAX_INFLIGHT_FETCHES, NameStatusEntry, parse_hunks, parse_name_status_entries,
};
use super::{GitOutput, run_git};

/// Unified-diff context lines each per-file fetch pins. git's own
/// default: enough to read a change in place, small enough that a
/// one-line edit to a huge file does not carry the file.
pub const CONTEXT_LINES: u32 = 3;

/// Carried lines per file. A generated file's rewrite must not carry its
/// whole body into a record that is re-sent on every subscribe.
pub const MAX_FILE_LINES: usize = 400;

/// Carried bytes per file - the cap a file of few but long lines meets
/// first.
pub const MAX_FILE_BYTES: usize = 32 * 1024;

/// Carried line text across the whole read, both layers together. The
/// JSON envelope the record adds beside it is not counted here; the
/// bound is on the diff's own content.
pub const MAX_TOTAL_BYTES: usize = 512 * 1024;

/// The most files the read fetches content for, both layers together.
/// Each fetch is a subprocess, so this is the read's work bound where
/// the byte caps are its payload bound; a file past it keeps its status
/// and is flagged truncated.
pub const MAX_FILES: usize = 100;

/// The changed files of a seat's two diff layers, bounded and flagged.
///
/// A layer answers in [`LayerState`]'s three states, exactly as the
/// snapshot's layer does: `Populated` carries the files, `Clean` says
/// the layer had nothing to show, `ScanFailed` says the read could not
/// be taken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffContent {
    pub worktree: LayerState<Vec<FileContent>>,
    pub branch_ahead: LayerState<Vec<FileContent>>,
    /// Content this read withheld: a file cut by a cap, a file past the
    /// file cap, or a fetch that failed. The per-file flags say which.
    pub truncated: bool,
}

/// One changed file's bounded content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileContent {
    pub path: String,
    /// The path a rename or copy came from.
    pub old_path: Option<String>,
    pub status: FileStatus,
    /// git reported the entry as binary: the status is the whole answer,
    /// and `hunks` is empty.
    pub binary: bool,
    /// The entry is a submodule (a gitlink): its content is two commit
    /// ids rather than file lines, so `hunks` is empty.
    pub submodule: bool,
    /// This file's content was withheld: cut by a per-file cap, past the
    /// file cap, or the fetch failed.
    pub truncated: bool,
    pub hunks: Vec<Hunk>,
}

/// The read's budget, spent as content is carried across both layers.
#[derive(Debug, Default)]
struct Caps {
    /// Files carried so far.
    files: usize,
    /// Content bytes carried so far.
    bytes: usize,
    /// Whether anything was withheld. Sticky.
    withheld: bool,
}

/// Read both layers of `snapshot` for `cwd` into bounded content.
///
/// Infallible like the stats scan: a layer's failure collapses to
/// [`LayerState::ScanFailed`], a file's to its own `truncated` flag, and
/// the read's to [`DiffContent::truncated`].
pub async fn scan<'a>(cwd: &'a Path, snapshot: &'a GitDiffSnapshot) -> DiffContent {
    let mut caps = Caps::default();
    let worktree = match &snapshot.worktree {
        LayerState::Populated(_) => scan_layer(cwd, "HEAD", &mut caps).await,
        LayerState::Clean => LayerState::Clean,
        LayerState::ScanFailed => LayerState::ScanFailed,
    };
    let branch_ahead = match &snapshot.branch_ahead {
        LayerState::Populated(_) => match snapshot.default_branch.as_deref() {
            Some(default) => scan_layer(cwd, &branch_range(default), &mut caps).await,
            // The snapshot's own invariant is that a populated layer has a
            // resolved default; a violated one is a failure to report, not
            // an empty answer.
            None => LayerState::ScanFailed,
        },
        LayerState::Clean => LayerState::Clean,
        LayerState::ScanFailed => LayerState::ScanFailed,
    };
    DiffContent { worktree, branch_ahead, truncated: caps.withheld }
}

/// Read one layer: its file list, then one bounded fetch per file.
///
/// **The fetches run a wave at a time, and each wave is folded as it
/// lands.** A section is a raw diff of up to the subprocess output cap,
/// so holding every completed one for the whole read would materialise
/// hundreds of megabytes to carry half a megabyte; a wave is the same
/// in-flight count the overlay's reader holds, and folding drops the raw
/// text before the next wave starts.
async fn scan_layer(cwd: &Path, ref_spec: &str, caps: &mut Caps) -> LayerState<Vec<FileContent>> {
    let entries = match run_git(cwd, &["diff", ref_spec, "--name-status"]).await {
        GitOutput::Ok(raw) => parse_name_status_entries(&raw),
        GitOutput::Empty => Vec::new(),
        GitOutput::Failed | GitOutput::Oversize => return LayerState::ScanFailed,
    };
    // Nothing changed is the layer's `Clean`, the state the snapshot
    // collapses a change-free layer to as well.
    if entries.is_empty() {
        return LayerState::Clean;
    }
    // Only the files the fold will still carry are fetched.
    let to_fetch = entries.len().min(MAX_FILES.saturating_sub(caps.files));
    let mut files = Vec::with_capacity(entries.len());
    for wave in entries[..to_fetch].chunks(MAX_INFLIGHT_FETCHES) {
        let sections = futures::future::join_all(
            wave.iter().map(|entry| fetch_section(cwd, ref_spec, &entry.path)),
        )
        .await;
        for (entry, section) in wave.iter().zip(sections) {
            files.push(fold_one(entry, section.as_deref(), caps));
        }
    }
    for entry in &entries[to_fetch..] {
        files.push(fold_one(entry, None, caps));
    }
    LayerState::Populated(files)
}

/// One file's diff section, `None` when git would not answer it (a crash
/// or a read past the subprocess cap). A binary body and an empty one
/// both answer `Some`.
async fn fetch_section(cwd: &Path, ref_spec: &str, path: &str) -> Option<String> {
    let context = format!("-U{CONTEXT_LINES}");
    match run_git(cwd, &["diff", ref_spec, "--no-ext-diff", &context, "--", path]).await {
        GitOutput::Ok(section) => Some(section),
        GitOutput::Empty => Some(String::new()),
        GitOutput::Failed | GitOutput::Oversize => None,
    }
}

/// One entry and its fetched section as a file row. `None` is a fetch
/// that was withheld or never made: the row keeps its status and says
/// its content is missing.
fn fold_one(entry: &NameStatusEntry, section: Option<&str>, caps: &mut Caps) -> FileContent {
    let carried = caps.files < MAX_FILES;
    let Some(section) = section.filter(|_| carried) else {
        caps.withheld = true;
        return FileContent {
            path: entry.path.clone(),
            old_path: entry.old_path.clone(),
            status: entry.status,
            binary: false,
            submodule: false,
            truncated: true,
            hunks: Vec::new(),
        };
    };
    caps.files += 1;
    let binary = section_is_binary(section);
    let submodule = !binary && section_is_submodule(section);
    let (hunks, cut) =
        if binary || submodule { (Vec::new(), false) } else { fold_hunks(section, caps) };
    FileContent {
        path: entry.path.clone(),
        old_path: entry.old_path.clone(),
        status: entry.status,
        binary,
        submodule,
        truncated: cut,
        hunks,
    }
}

/// Carry one section's hunks under the per-file caps and the read's
/// total, answering what is carried and whether anything was cut.
///
/// A cut lands mid-hunk: the carried part keeps the hunk's start and
/// takes counts describing the lines it actually carries, so a renderer
/// reads a header that matches its body.
fn fold_hunks(section: &str, caps: &mut Caps) -> (Vec<Hunk>, bool) {
    let mut carried = Vec::new();
    let mut file_lines = 0usize;
    let mut file_bytes = 0usize;
    let mut cut = false;
    for hunk in parse_hunks(section) {
        let total = hunk.lines.len();
        let mut kept = Vec::new();
        let mut old_count = 0u32;
        let mut new_count = 0u32;
        let mut stopped = false;
        for line in &hunk.lines {
            let cost = line.text.len() + 1;
            if file_lines + 1 > MAX_FILE_LINES
                || file_bytes + cost > MAX_FILE_BYTES
                || caps.bytes + cost > MAX_TOTAL_BYTES
            {
                stopped = true;
                break;
            }
            file_lines += 1;
            file_bytes += cost;
            caps.bytes += cost;
            if line.old_line.is_some() {
                old_count += 1;
            }
            if line.new_line.is_some() {
                new_count += 1;
            }
            kept.push(line.clone());
        }
        if kept.len() == total {
            carried.push(hunk);
        } else if !kept.is_empty() {
            carried.push(Hunk {
                old_start: hunk.old_start,
                old_count,
                new_start: hunk.new_start,
                new_count,
                lines: kept,
            });
        }
        if stopped {
            cut = true;
            break;
        }
    }
    if cut {
        caps.withheld = true;
    }
    (carried, cut)
}

/// git prints one of these instead of hunks when it will not diff the
/// bytes - on both sides, or the null side of a new/deleted file.
fn section_is_binary(section: &str) -> bool {
    section.lines().any(|line| line.starts_with("Binary files "))
}

/// A gitlink: git prints the mode 160000 on one of the entry's header
/// lines (`index a..b 160000`, `new file mode 160000`, `old mode
/// 160000`), and the body is a pair of commit ids.
fn section_is_submodule(section: &str) -> bool {
    section
        .lines()
        .take_while(|line| !line.starts_with("@@"))
        .any(|line| line.split_whitespace().last() == Some("160000"))
}

/// The ref range the branch-ahead layer is read with: the merge-base
/// form the stats scan composes (`<default>...HEAD`), so a file's row
/// and its content stay one tree after a squash-merge.
fn branch_range(default_branch: &str) -> String {
    format!("{default_branch}...HEAD")
}

#[cfg(test)]
mod tests {
    use std::process::Command as StdCommand;

    use tempfile::TempDir;

    use super::super::hunks::DiffLineKind;
    use super::super::scan as scan_stats;
    use super::*;
    use forge_primitives::git::GitBranch;
    use forge_primitives::git_diff::RepoGate;

    const MODIFIED: &str = r"diff --git a/x.rs b/x.rs
index 1111111..2222222 100644
--- a/x.rs
+++ b/x.rs
@@ -1,3 +1,4 @@
 line1
 line2
+new line
 line3
";

    const DELETED: &str = r"diff --git a/gone.rs b/gone.rs
deleted file mode 100644
index 1111111..0000000
--- a/gone.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-old one
-old two
";

    const BINARY: &str = r"diff --git a/blob.bin b/blob.bin
new file mode 100644
index 0000000..2222222
Binary files /dev/null and b/blob.bin differ
";

    const SUBMODULE: &str = r"diff --git a/sub b/sub
index 1111111..2222222 160000
--- a/sub
+++ b/sub
@@ -1 +1 @@
-Subproject commit 1111111111111111111111111111111111111111
+Subproject commit 2222222222222222222222222222222222222222
";

    /// One modified-path entry, the shape `--name-status` yields for it.
    fn entry(path: &str) -> NameStatusEntry {
        NameStatusEntry { status: FileStatus::Modified, path: path.to_owned(), old_path: None }
    }

    /// A one-hunk section carrying `lines` as additions, under a caller's
    /// own hunk header.
    fn section_with_header(header: &str, lines: &[String]) -> String {
        let mut body = String::new();
        for line in lines {
            body.push('+');
            body.push_str(line);
            body.push('\n');
        }
        format!(
            "diff --git a/x.rs b/x.rs\nindex 1111111..2222222 100644\n--- a/x.rs\n+++ b/x.rs\n{header}\n{body}"
        )
    }

    /// [`section_with_header`] under the plain all-added header.
    fn added_section(lines: &[String]) -> String {
        section_with_header(&format!("@@ -0,0 +1,{} @@", lines.len()), lines)
    }

    fn populated(layer: LayerState<Vec<FileContent>>) -> Vec<FileContent> {
        let LayerState::Populated(files) = layer else {
            panic!("the layer is populated, got {layer:?}");
        };
        files
    }

    fn carried_lines(files: &[FileContent]) -> usize {
        files.iter().flat_map(|file| &file.hunks).map(|hunk| hunk.lines.len()).sum()
    }

    /// One layer's content, folded in entry order: the same per-file
    /// `fold_one` the scan's wave loop calls, driven from fixtures here
    /// with every section already in hand.
    fn fold_layer(
        entries: &[NameStatusEntry],
        sections: &[Option<String>],
        caps: &mut Caps,
    ) -> LayerState<Vec<FileContent>> {
        let files = entries
            .iter()
            .enumerate()
            .map(|(idx, entry)| fold_one(entry, sections.get(idx).and_then(Option::as_deref), caps))
            .collect();
        LayerState::Populated(files)
    }

    // ---- the fold, from fixtures ----

    /// A modification carries the parser's lines as `git diff` printed
    /// them: the status, the path, and one hunk of context and addition.
    #[test]
    fn fold_carries_a_modified_files_lines() {
        let entries = [entry("x.rs")];
        let sections = [Some(MODIFIED.to_owned())];
        let mut caps = Caps::default();

        let files = populated(fold_layer(&entries, &sections, &mut caps));

        assert_eq!(files.len(), 1, "one entry, one row");
        let file = &files[0];
        assert_eq!(file.path, "x.rs", "the row carries the path `--name-status` named");
        assert_eq!(file.status, FileStatus::Modified, "and the status it classified");
        assert!(!file.truncated, "a small file is not flagged");
        assert!(!caps.withheld, "and the read withholds nothing");
        assert_eq!(file.hunks.len(), 1, "one section, one hunk");
        assert_eq!(file.hunks[0].lines.len(), 4, "context, context, added, context");
        assert!(
            file.hunks[0]
                .lines
                .iter()
                .any(|line| line.kind == DiffLineKind::Added && line.text == "new line"),
            "the added line arrives with its kind and its text",
        );
    }

    /// A deletion carries its removed lines.
    #[test]
    fn fold_carries_a_deleted_files_removed_lines() {
        let entries = [NameStatusEntry {
            status: FileStatus::Deleted,
            path: "gone.rs".to_owned(),
            old_path: None,
        }];
        let sections = [Some(DELETED.to_owned())];
        let mut caps = Caps::default();

        let files = populated(fold_layer(&entries, &sections, &mut caps));

        let file = &files[0];
        assert_eq!(file.status, FileStatus::Deleted, "the row keeps the deletion's status");
        assert!(!file.truncated, "and a small deletion is not flagged");
        let removed: Vec<&str> = file.hunks[0]
            .lines
            .iter()
            .filter(|line| line.kind == DiffLineKind::Removed)
            .map(|line| line.text.as_str())
            .collect();
        assert_eq!(removed, vec!["old one", "old two"], "both removed lines arrive, in order");
    }

    /// A rename row keeps both sides: the new path the status names, and
    /// the old path only `--name-status` still carries.
    #[test]
    fn a_rename_row_carries_the_path_it_came_from() {
        let entries = parse_name_status_entries("R100\told.rs\tnew.rs");
        let sections = [Some(added_section(&["still here".to_owned()]))];
        let mut caps = Caps::default();

        let files = populated(fold_layer(&entries, &sections, &mut caps));

        let file = &files[0];
        assert_eq!(file.status, FileStatus::Renamed, "the rename's own status crosses");
        assert_eq!(file.path, "new.rs", "with the new path as the row's");
        assert_eq!(file.old_path.as_deref(), Some("old.rs"), "and the old path kept beside it");
        assert_eq!(file.hunks.len(), 1, "the moved file still carries its change");
    }

    /// A binary file is its status and its flag: no lines exist to carry,
    /// and the empty body must not read as an empty change.
    #[test]
    fn a_binary_section_is_flagged_and_carries_no_hunks() {
        let entries = [entry("blob.bin")];
        let sections = [Some(BINARY.to_owned())];
        let mut caps = Caps::default();

        let files = populated(fold_layer(&entries, &sections, &mut caps));

        let file = &files[0];
        assert!(file.binary, "the binary flag is what says the body is not an empty change");
        assert!(!file.submodule, "and a blob is not the gitlink case");
        assert!(!file.truncated, "nothing was cut: the file simply has no lines");
        assert!(file.hunks.is_empty(), "no lines exist to carry for a binary");
        assert!(!caps.withheld, "and no content was withheld");
    }

    /// A submodule entry is flagged the same way: its "content" is two
    /// commit ids, not file lines.
    #[test]
    fn a_submodule_section_is_flagged_and_carries_no_hunks() {
        let entries = [entry("sub")];
        let sections = [Some(SUBMODULE.to_owned())];
        let mut caps = Caps::default();

        let files = populated(fold_layer(&entries, &sections, &mut caps));

        let file = &files[0];
        assert!(file.submodule, "the gitlink's mode is what flags it");
        assert!(!file.binary, "and a gitlink is not the binary case");
        assert!(!file.truncated, "nothing was cut: the shas are simply not carried");
        assert!(file.hunks.is_empty(), "a sha bump is not file content");
    }

    /// The per-file line cap cuts mid-hunk, recomputes the kept hunk's
    /// counts to what is carried, and flags the file and the read.
    #[test]
    fn the_per_file_line_cap_cuts_and_flags_the_file() {
        let added: Vec<String> = (0..MAX_FILE_LINES + 5).map(|i| format!("line {i}")).collect();
        let entries = [entry("x.rs")];
        // A header with starts of its own, so the cut's survival of them is
        // visible rather than implied by a zero.
        let sections = [Some(section_with_header("@@ -61,405 +77,410 @@", &added))];
        let mut caps = Caps::default();

        let files = populated(fold_layer(&entries, &sections, &mut caps));

        let file = &files[0];
        assert!(
            file.truncated,
            "a file past the 400-line per-file cap is flagged (MAX_FILE_LINES is 400)",
        );
        assert!(caps.withheld, "and the read says content was withheld");
        assert_eq!(carried_lines(&files), 400, "exactly the 400-line cap is carried");
        let kept = &file.hunks[0];
        assert_eq!(kept.lines.len(), 400, "the surviving hunk is the carried lines alone");
        assert_eq!(kept.new_count, 400, "the kept hunk's counts describe what is carried");
        assert_eq!(kept.old_count, 0, "and the old side counts only the removed lines carried");
        assert_eq!(kept.old_start, 61, "with the hunk's own old start kept");
        assert_eq!(kept.new_start, 77, "and its own new start");
    }

    /// The per-file byte cap cuts a file of few but long lines well
    /// before its line count reaches the line cap.
    #[test]
    fn the_per_file_byte_cap_cuts_and_flags_the_file() {
        let line = "x".repeat(200);
        let added: Vec<String> = (0..MAX_FILE_LINES).map(|_| line.clone()).collect();
        let entries = [entry("x.rs")];
        let sections = [Some(added_section(&added))];
        let mut caps = Caps::default();

        let files = populated(fold_layer(&entries, &sections, &mut caps));

        let file = &files[0];
        assert!(
            file.truncated,
            "200-byte lines pass the 32 KiB per-file byte cap (MAX_FILE_BYTES is 32768)",
        );
        assert_eq!(carried_lines(&files), 163, "163 lines of 201 bytes fit the 32 KiB cap");
        assert!(carried_lines(&files) < MAX_FILE_LINES, "the byte cap, not the line cap, cut this");
        assert!(caps.withheld, "and the read says content was withheld");
    }

    /// The total cap is the read's own, not a file's: with the budget
    /// nearly spent, the next file is cut and the one after still lists.
    #[test]
    fn the_total_cap_stops_the_read_across_files() {
        let line = "y".repeat(30);
        let added: Vec<String> = (0..3).map(|_| line.clone()).collect();
        let entries = [entry("a.rs"), entry("b.rs")];
        let sections = [Some(added_section(&added)), Some(added_section(&added))];
        // 31 bytes of the 512 KiB total are left: exactly one 31-byte line.
        let mut caps = Caps { bytes: 512 * 1024 - 31, ..Caps::default() };

        let files = populated(fold_layer(&entries, &sections, &mut caps));

        assert!(files[0].truncated, "the first file is cut by the total cap");
        assert_eq!(carried_lines(&files[..1]), 1, "one 31-byte line fits the remaining budget");
        assert!(files[1].truncated, "and the next file carries nothing");
        assert!(files[1].hunks.is_empty(), "the exhausted budget carries no lines for it");
        assert!(caps.withheld, "the read says its content was withheld");
        assert!(caps.bytes <= 512 * 1024, "the read never carries past its 512 KiB total");
    }

    /// The file cap is a count across the read: past it a row keeps its
    /// status and is flagged, and no content rides for it.
    #[test]
    fn the_file_cap_leaves_the_rest_listed_with_no_content() {
        let entries: Vec<NameStatusEntry> =
            (0..=MAX_FILES).map(|i| entry(&format!("f{i}.rs"))).collect();
        let sections: Vec<Option<String>> =
            (0..=MAX_FILES).map(|_| Some(added_section(&["one".to_owned()]))).collect();
        let mut caps = Caps::default();

        let files = populated(fold_layer(&entries, &sections, &mut caps));

        assert_eq!(files.len(), MAX_FILES + 1, "every file is still listed");
        assert_eq!(MAX_FILES, 100, "the read's file cap is the 100 socket.md states");
        let last_carried = files.get(MAX_FILES - 1).expect("the 100th file rides within the cap");
        let first_withheld = files.get(MAX_FILES).expect("the 101st file is still listed");
        assert!(!last_carried.truncated, "the 100th file is within the 100-file cap");
        assert!(first_withheld.truncated, "the 101st is past the 100-file cap");
        assert!(first_withheld.hunks.is_empty(), "and carries no content");
        assert_eq!(first_withheld.path, format!("f{MAX_FILES}.rs"), "while keeping its status row");
        assert!(caps.withheld, "and the read says content was withheld");
    }

    /// A file whose fetch was withheld is listed and flagged, never
    /// dropped.
    #[test]
    fn a_file_whose_fetch_was_withheld_is_listed_and_flagged() {
        let entries = [entry("kept.rs"), entry("lost.rs")];
        let sections = [Some(added_section(&["one".to_owned()])), None];
        let mut caps = Caps::default();

        let files = populated(fold_layer(&entries, &sections, &mut caps));

        assert_eq!(files.len(), 2, "both rows are listed");
        assert!(!files[0].truncated, "the fetched file carries its content unflagged");
        assert!(files[1].truncated, "a withheld fetch flags the file");
        assert!(files[1].hunks.is_empty(), "and it carries no lines");
        assert_eq!(files[1].path, "lost.rs", "while its own path stays on the row");
        assert!(caps.withheld, "and the read withholds");
    }

    /// The branch layer compares against the merge base - the same
    /// three-dot range the stats scan's branch-ahead layer uses - so a
    /// squash-merge cannot empty it where a two-dot diff would.
    #[test]
    fn the_branch_layer_compares_against_the_merge_base() {
        assert_eq!(branch_range("main"), "main...HEAD", "a local default reads three-dot");
        assert_eq!(
            branch_range("origin/main"),
            "origin/main...HEAD",
            "and so does a remote-tracking one",
        );
    }

    // ---- a real repository, end to end ----

    fn git(dir: &TempDir, args: &[&str]) {
        let out =
            StdCommand::new("git").arg("-C").arg(dir.path()).args(args).output().expect("run git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn init_repo(dir: &TempDir, branch: &str) {
        git(dir, &["init", "-q"]);
        git(dir, &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
    }

    fn write_file(dir: &TempDir, name: &str, contents: &str) {
        std::fs::write(dir.path().join(name), contents).expect("write file");
    }

    fn commit_all(dir: &TempDir, message: &str) {
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", message]);
    }

    /// The worktree layer reads what the dirty tree changed: a
    /// modification, a deletion, an addition and a binary, each with the
    /// status and the flag that describe it.
    #[tokio::test(flavor = "current_thread")]
    async fn a_dirty_tree_reads_its_worktree_layer() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        write_file(&dir, "kept.rs", "one\ntwo\n");
        write_file(&dir, "gone.rs", "old one\nold two\n");
        commit_all(&dir, "base");
        write_file(&dir, "kept.rs", "one\ntwo\nthree\n");
        git(&dir, &["rm", "-q", "gone.rs"]);
        write_file(&dir, "added.rs", "new\n");
        std::fs::write(dir.path().join("blob.bin"), b"\0\0binary\0").expect("binary write");
        git(&dir, &["add", "added.rs", "blob.bin"]);

        let snapshot = scan_stats(dir.path(), None).await;
        let content = scan(dir.path(), &snapshot).await;

        assert!(
            matches!(content.branch_ahead, LayerState::Clean),
            "on the default branch there is no branch layer",
        );
        let files = populated(content.worktree);
        let row = |path: &str| files.iter().find(|file| file.path == path).expect(path);
        let lines = |path: &str| -> Vec<(DiffLineKind, String)> {
            row(path)
                .hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .map(|line| (line.kind, line.text.clone()))
                .collect()
        };
        assert_eq!(row("kept.rs").status, FileStatus::Modified);
        assert!(
            lines("kept.rs").contains(&(DiffLineKind::Added, "three".to_owned())),
            "the modification carries its added line",
        );
        assert_eq!(row("gone.rs").status, FileStatus::Deleted);
        assert!(
            lines("gone.rs").contains(&(DiffLineKind::Removed, "old one".to_owned())),
            "the deletion carries its removed lines",
        );
        assert_eq!(row("added.rs").status, FileStatus::Added);
        assert!(row("blob.bin").binary, "a NUL-carrying file is the binary case");
        assert!(row("blob.bin").hunks.is_empty());
        assert!(!content.truncated, "a handful of small files withholds nothing");
    }

    /// A feature branch with uncommitted edits reads both layers, with
    /// the refs the stats scan used: `HEAD` for the worktree, the merge
    /// base for the branch.
    #[tokio::test(flavor = "current_thread")]
    async fn a_feature_branch_reads_both_layers() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        write_file(&dir, "base.rs", "base\n");
        commit_all(&dir, "base");
        git(&dir, &["checkout", "-q", "-b", "feat/x"]);
        write_file(&dir, "feat.rs", "fn x() {}\n");
        commit_all(&dir, "feat");
        write_file(&dir, "base.rs", "base\nedited\n");

        let snapshot = scan_stats(dir.path(), None).await;
        let content = scan(dir.path(), &snapshot).await;

        let worktree = populated(content.worktree);
        assert_eq!(worktree.len(), 1);
        assert_eq!(worktree[0].path, "base.rs", "the worktree layer is the uncommitted edit");
        let ahead = populated(content.branch_ahead);
        assert_eq!(ahead.len(), 1);
        assert_eq!(ahead[0].path, "feat.rs", "the branch layer is the commit the branch added");
        assert_eq!(ahead[0].status, FileStatus::Added);
        assert!(!content.truncated);
    }

    /// The read's future must be `Send`: the socket's record encode awaits
    /// it in a task the upgrade demands `Send` for. A regression to the
    /// buffered stream the overlay fetches with fails here at compile time
    /// with the compiler's misleading #100013 lifetime error, which is
    /// exactly the trap this pin exists to name.
    #[test]
    fn the_read_future_is_send() {
        fn assert_send<T: Send>(_: T) {}
        let snapshot = GitDiffSnapshot {
            branch: GitBranch::Named("main".to_owned()),
            default_branch: Some("main".to_owned()),
            repo_gate: RepoGate::InRepo,
            pushed_sha: None,
            worktree: LayerState::Clean,
            branch_ahead: LayerState::Clean,
            pr: None,
            closes: Vec::new(),
            pr_fetched_at: None,
        };
        assert_send(scan(std::path::Path::new("/"), &snapshot));
    }

    /// The wire spellings `socket.md`'s `diff` row names: a client narrows
    /// these tags, so one of them moving is a wire change the book and
    /// every client would have to follow, and the fixture would follow it
    /// silently.
    #[test]
    fn the_wire_spellings_are_the_ones_the_book_names() {
        let clean: LayerState<Vec<FileContent>> = LayerState::Clean;
        assert_eq!(serde_json::to_value(&clean).expect("encodes"), serde_json::json!("clean"));
        let failed: LayerState<Vec<FileContent>> = LayerState::ScanFailed;
        assert_eq!(
            serde_json::to_value(&failed).expect("encodes"),
            serde_json::json!("scan_failed"),
        );
        assert_eq!(
            serde_json::to_value(FileStatus::Typechange).expect("encodes"),
            serde_json::json!("typechange"),
        );
        assert_eq!(
            serde_json::to_value(DiffLineKind::Removed).expect("encodes"),
            serde_json::json!("removed"),
        );
    }

    /// The per-file fetch pins the three context lines this read
    /// declares: a change in a long file carries the change and a little
    /// around it, not the file.
    #[tokio::test(flavor = "current_thread")]
    async fn a_fetch_pins_the_context_the_read_declares() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        let base: Vec<String> = (1..=9).map(|i| format!("line {i}")).collect();
        write_file(&dir, "long.rs", &format!("{}\n", base.join("\n")));
        commit_all(&dir, "base");
        let edited: Vec<String> = (1..=9)
            .map(|i| if i == 5 { "line 5 CHANGED".to_owned() } else { format!("line {i}") })
            .collect();
        write_file(&dir, "long.rs", &format!("{}\n", edited.join("\n")));

        let snapshot = scan_stats(dir.path(), None).await;
        let content = scan(dir.path(), &snapshot).await;

        let files = populated(content.worktree);
        let file = files.iter().find(|file| file.path == "long.rs").expect("long.rs");
        assert_eq!(
            carried_lines(std::slice::from_ref(file)),
            8,
            "3 context above + the change + 3 below: CONTEXT_LINES is 3",
        );
    }

    /// A file past the per-file line cap flags the file AND the read,
    /// through the real fetch path: every other truncation fixture drives
    /// `fold_layer` directly, so this is the one that pins the read-level
    /// flag's true side end to end.
    #[tokio::test(flavor = "current_thread")]
    async fn a_file_past_the_line_cap_flags_the_file_and_the_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        let base: Vec<String> = (1..=MAX_FILE_LINES + 20).map(|i| format!("line {i}")).collect();
        write_file(&dir, "long.rs", &format!("{}\n", base.join("\n")));
        commit_all(&dir, "base");
        // Every line edited, so the diff is ~2x the file and passes the cap.
        let edited: Vec<String> = base.iter().map(|line| format!("{line} edited")).collect();
        write_file(&dir, "long.rs", &format!("{}\n", edited.join("\n")));

        let snapshot = scan_stats(dir.path(), None).await;
        let content = scan(dir.path(), &snapshot).await;

        assert!(content.truncated, "the read says its content was withheld");
        let files = populated(content.worktree);
        let file = files.iter().find(|file| file.path == "long.rs").expect("long.rs");
        assert!(file.truncated, "and the file says its own content was cut");
        assert_eq!(
            carried_lines(std::slice::from_ref(file)),
            MAX_FILE_LINES,
            "the carried lines stop at the per-file cap",
        );
    }

    /// The tree can go clean between the stats scan and the content
    /// read: the layer answers `Clean` rather than an empty populated
    /// list, the same state the snapshot collapses a change-free layer
    /// to.
    #[tokio::test(flavor = "current_thread")]
    async fn a_layer_that_went_clean_between_reads_answers_clean() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        write_file(&dir, "kept.rs", "one\n");
        commit_all(&dir, "base");
        write_file(&dir, "kept.rs", "one\ntwo\n");

        let snapshot = scan_stats(dir.path(), None).await;
        assert!(snapshot.worktree.is_populated(), "the dirty tree is the premise");
        // Cleaned between the two reads, so the content read finds nothing.
        write_file(&dir, "kept.rs", "one\n");

        let content = scan(dir.path(), &snapshot).await;

        assert!(matches!(content.worktree, LayerState::Clean));
        assert!(!content.truncated);
    }

    /// The wave seam, crossed: more files than one wave holds, with the
    /// first wave spending the read's whole total, so the file over the
    /// seam can only be cut by a budget that survived it - a per-wave
    /// `Caps` reset would carry it whole - and the rows can only come
    /// back in `--name-status` order.
    #[tokio::test(flavor = "current_thread")]
    async fn the_read_carries_its_budget_and_order_across_the_wave_seam() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        write_file(&dir, "base.txt", "base\n");
        commit_all(&dir, "base");

        // A full wave of files whose diffs each pass the per-file byte cap:
        // sixteen files carrying 81 of 401-byte lines each spend 519,696 of
        // the 524,288-byte total, leaving 4,592.
        let wide = format!("{}\n", "x".repeat(400));
        for i in 0..MAX_INFLIGHT_FETCHES {
            let name = format!("w1-{i:02}.rs");
            write_file(&dir, &name, &wide.repeat(100));
        }
        // Sorted last, so it lands in the second wave: 100 of 101-byte
        // lines, past every per-file cap and only the total can cut it, at
        // the 45 lines 4,592 bytes hold.
        let seam = "z-seam.rs";
        write_file(&dir, seam, &format!("{}\n", "y".repeat(100)).repeat(100));
        git(&dir, &["add", "-A"]);

        let snapshot = scan_stats(dir.path(), None).await;
        let content = scan(dir.path(), &snapshot).await;

        assert!(content.truncated, "the read says its content was withheld");
        let files = populated(content.worktree);
        assert_eq!(files.len(), MAX_INFLIGHT_FETCHES + 1, "every changed file is listed");
        let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths[0], "w1-00.rs", "the rows come back in `--name-status` order");
        assert_eq!(paths[MAX_INFLIGHT_FETCHES - 1], "w1-15.rs", "through the first wave");
        assert_eq!(paths[MAX_INFLIGHT_FETCHES], seam, "and across the seam, still in order");

        let carried: usize = files
            .iter()
            .flat_map(|file| &file.hunks)
            .flat_map(|hunk| &hunk.lines)
            .map(|line| line.text.len() + 1)
            .sum();
        assert!(carried <= 512 * 1024, "the total never carries past its 512 KiB");
        let over = files.last().expect("the seam file is the last row");
        assert!(over.truncated, "the seam file is cut by the budget the first wave left");
        assert_eq!(
            carried_lines(std::slice::from_ref(over)),
            45,
            "with exactly the 45 lines the 4,592 bytes past the seam hold",
        );
        assert!(
            files[..MAX_INFLIGHT_FETCHES].iter().all(|file| file.truncated),
            "and every first-wave file is cut by the 32 KiB per-file byte cap",
        );
    }

    /// A clean tree on the default branch has nothing to read in either
    /// layer.
    #[tokio::test(flavor = "current_thread")]
    async fn a_clean_tree_reads_no_layer() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        write_file(&dir, "base.rs", "base\n");
        commit_all(&dir, "base");

        let snapshot = scan_stats(dir.path(), None).await;
        let content = scan(dir.path(), &snapshot).await;

        assert!(matches!(content.worktree, LayerState::Clean));
        assert!(matches!(content.branch_ahead, LayerState::Clean));
        assert!(!content.truncated);
    }

    /// Outside a repository both layers are empty, and the gate on the
    /// work row - not this read - is what says why.
    #[tokio::test(flavor = "current_thread")]
    async fn outside_a_repository_reads_no_layer() {
        let dir = tempfile::tempdir().expect("tempdir");

        let snapshot = scan_stats(dir.path(), None).await;
        assert_eq!(snapshot.repo_gate, RepoGate::NotARepo);
        let content = scan(dir.path(), &snapshot).await;

        assert!(matches!(content.worktree, LayerState::Clean));
        assert!(matches!(content.branch_ahead, LayerState::Clean));
        assert!(!content.truncated);
    }

    /// A layer the stats scan could not read is mirrored as failed - the
    /// content read does not quietly answer it empty.
    #[tokio::test(flavor = "current_thread")]
    async fn a_scan_failed_layer_is_mirrored() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        let snapshot = GitDiffSnapshot {
            branch: GitBranch::Named("main".to_owned()),
            default_branch: Some("main".to_owned()),
            repo_gate: RepoGate::InRepo,
            pushed_sha: None,
            worktree: LayerState::ScanFailed,
            branch_ahead: LayerState::Clean,
            pr: None,
            closes: Vec::new(),
            pr_fetched_at: None,
        };

        let content = scan(dir.path(), &snapshot).await;

        assert!(matches!(content.worktree, LayerState::ScanFailed), "a failed layer stays failed");
        assert!(matches!(content.branch_ahead, LayerState::Clean));
    }
}
