//! Row-pairing for the split-view diff renderer.
//!
//! A unified-diff hunk lists lines in source order: context lines
//! interleaved with consecutive runs of removed (`-`) and added (`+`)
//! lines. The split renderer wants rows where the left and right
//! columns line up — same context lines in both, paired modifications
//! side-by-side, standalone removals or additions filling one column
//! with a blank in the other.
//!
//! The algorithm walks lines once, buffering removed + added runs. On
//! a context line or end-of-hunk, it zips the buffers into pairs:
//! `min(|R|,|A|)` paired modifications first, then standalones for
//! whichever side has excess.

use crate::app::diff_overlay::LineKey;
use forge_workspace::env::git_diff::hunks::{DiffLine, DiffLineKind};

/// One row in the split renderer's body. At least one side is `Some`
/// (a fully-empty pair is never emitted). Context lines produce a
/// `(Some, Some)` pair where both sides reference the same source
/// `DiffLine`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PairedDiffRow {
    pub left: Option<LineKey>,
    pub right: Option<LineKey>,
}

/// Walk a single hunk's lines and emit row pairs. `file_idx` /
/// `hunk_idx` are stamped onto each `LineKey` so the renderer's
/// downstream lookup (`files[file_idx].hunks[hunk_idx].lines[line_idx]`)
/// resolves without extra bookkeeping.
pub(crate) fn pair_hunk_lines(
    file_idx: usize,
    hunk_idx: usize,
    lines: &[DiffLine],
) -> Vec<PairedDiffRow> {
    let mut out = Vec::with_capacity(lines.len());
    let mut removed_buf: Vec<usize> = Vec::new();
    let mut added_buf: Vec<usize> = Vec::new();

    for (line_idx, line) in lines.iter().enumerate() {
        match line.kind {
            DiffLineKind::Removed => removed_buf.push(line_idx),
            DiffLineKind::Added => added_buf.push(line_idx),
            DiffLineKind::Context => {
                flush_change(&mut out, file_idx, hunk_idx, &mut removed_buf, &mut added_buf);
                let key = LineKey { file_idx, hunk_idx, line_idx };
                out.push(PairedDiffRow { left: Some(key), right: Some(key) });
            }
        }
    }
    flush_change(&mut out, file_idx, hunk_idx, &mut removed_buf, &mut added_buf);
    out
}

fn flush_change(
    out: &mut Vec<PairedDiffRow>,
    file_idx: usize,
    hunk_idx: usize,
    removed: &mut Vec<usize>,
    added: &mut Vec<usize>,
) {
    let paired = removed.len().min(added.len());
    for i in 0..paired {
        out.push(PairedDiffRow {
            left: Some(LineKey { file_idx, hunk_idx, line_idx: removed[i] }),
            right: Some(LineKey { file_idx, hunk_idx, line_idx: added[i] }),
        });
    }
    for &line_idx in removed.iter().skip(paired) {
        out.push(PairedDiffRow {
            left: Some(LineKey { file_idx, hunk_idx, line_idx }),
            right: None,
        });
    }
    for &line_idx in added.iter().skip(paired) {
        out.push(PairedDiffRow {
            left: None,
            right: Some(LineKey { file_idx, hunk_idx, line_idx }),
        });
    }
    removed.clear();
    added.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dl(kind: DiffLineKind, text: &str) -> DiffLine {
        DiffLine { kind, text: text.to_owned(), old_line: None, new_line: None }
    }

    fn k(file_idx: usize, hunk_idx: usize, line_idx: usize) -> Option<LineKey> {
        Some(LineKey { file_idx, hunk_idx, line_idx })
    }

    #[test]
    fn pure_context_pairs_left_and_right_to_same_line() {
        let lines = vec![
            dl(DiffLineKind::Context, "a"),
            dl(DiffLineKind::Context, "b"),
            dl(DiffLineKind::Context, "c"),
        ];
        let out = pair_hunk_lines(0, 0, &lines);
        assert_eq!(out.len(), 3);
        for (i, row) in out.iter().enumerate() {
            assert_eq!(row.left, k(0, 0, i));
            assert_eq!(row.right, k(0, 0, i));
        }
    }

    #[test]
    fn balanced_modification_pairs_removed_with_added() {
        let lines = vec![
            dl(DiffLineKind::Removed, "old1"),
            dl(DiffLineKind::Removed, "old2"),
            dl(DiffLineKind::Added, "new1"),
            dl(DiffLineKind::Added, "new2"),
        ];
        let out = pair_hunk_lines(0, 0, &lines);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], PairedDiffRow { left: k(0, 0, 0), right: k(0, 0, 2) });
        assert_eq!(out[1], PairedDiffRow { left: k(0, 0, 1), right: k(0, 0, 3) });
    }

    #[test]
    fn unbalanced_modification_leaves_remainder_as_standalone() {
        // 1 removed, 3 added → 1 paired + 2 right-only
        let lines = vec![
            dl(DiffLineKind::Removed, "old1"),
            dl(DiffLineKind::Added, "new1"),
            dl(DiffLineKind::Added, "new2"),
            dl(DiffLineKind::Added, "new3"),
        ];
        let out = pair_hunk_lines(0, 0, &lines);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], PairedDiffRow { left: k(0, 0, 0), right: k(0, 0, 1) });
        assert_eq!(out[1], PairedDiffRow { left: None, right: k(0, 0, 2) });
        assert_eq!(out[2], PairedDiffRow { left: None, right: k(0, 0, 3) });
    }

    #[test]
    fn pure_removal_is_left_only_rows() {
        let lines = vec![dl(DiffLineKind::Removed, "old1"), dl(DiffLineKind::Removed, "old2")];
        let out = pair_hunk_lines(0, 0, &lines);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], PairedDiffRow { left: k(0, 0, 0), right: None });
        assert_eq!(out[1], PairedDiffRow { left: k(0, 0, 1), right: None });
    }

    #[test]
    fn pure_addition_is_right_only_rows() {
        let lines = vec![dl(DiffLineKind::Added, "new1"), dl(DiffLineKind::Added, "new2")];
        let out = pair_hunk_lines(0, 0, &lines);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], PairedDiffRow { left: None, right: k(0, 0, 0) });
        assert_eq!(out[1], PairedDiffRow { left: None, right: k(0, 0, 1) });
    }

    #[test]
    fn context_flushes_pending_change_before_pairing_itself() {
        // ctx, ctx, -, +, +, ctx
        // → ctx, ctx, (pair -,+), (None,+), ctx
        let lines = vec![
            dl(DiffLineKind::Context, "a"),
            dl(DiffLineKind::Context, "b"),
            dl(DiffLineKind::Removed, "old"),
            dl(DiffLineKind::Added, "new1"),
            dl(DiffLineKind::Added, "new2"),
            dl(DiffLineKind::Context, "c"),
        ];
        let out = pair_hunk_lines(0, 0, &lines);
        assert_eq!(out.len(), 5);
        assert_eq!(out[0], PairedDiffRow { left: k(0, 0, 0), right: k(0, 0, 0) });
        assert_eq!(out[1], PairedDiffRow { left: k(0, 0, 1), right: k(0, 0, 1) });
        assert_eq!(out[2], PairedDiffRow { left: k(0, 0, 2), right: k(0, 0, 3) });
        assert_eq!(out[3], PairedDiffRow { left: None, right: k(0, 0, 4) });
        assert_eq!(out[4], PairedDiffRow { left: k(0, 0, 5), right: k(0, 0, 5) });
    }

    #[test]
    fn carries_file_and_hunk_indices_through() {
        let lines = vec![dl(DiffLineKind::Added, "x")];
        let out = pair_hunk_lines(3, 7, &lines);
        assert_eq!(
            out[0],
            PairedDiffRow {
                left: None,
                right: Some(LineKey { file_idx: 3, hunk_idx: 7, line_idx: 0 })
            }
        );
    }

    #[test]
    fn empty_hunk_emits_no_rows() {
        let out = pair_hunk_lines(0, 0, &[]);
        assert!(out.is_empty());
    }
}
