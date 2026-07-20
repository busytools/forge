//! Re-anchoring resolver for durable review threads.
//!
//! A persisted [`ReviewAnchor`] stores a drift-robust location (path +
//! side + line + content hash + a few context lines). The diff is
//! re-scanned on every overlay open, so the anchor must be re-resolved
//! to a fresh positional index into the new [`FileHunks`].
//! [`resolve_anchor`] reports whether the line is still in place, moved
//! a few lines, or gone.

use forge_primitives::review::{ReviewAnchor, ReviewSide};

use super::hunks::{DiffLine, FileHunks, Hunk};

/// Furthest a line may drift (in side line numbers) and still re-anchor.
/// Beyond this the thread is `Outdated` rather than risk matching an
/// unrelated same-content line far away.
const REANCHOR_WINDOW: i64 = 25;

/// Lines captured on each side of an anchored line for fuzzy re-anchor
/// disambiguation.
pub const CONTEXT_RADIUS: usize = 3;

/// Where a re-resolved anchor landed against a fresh diff scan. Indices
/// address `files[file_idx].hunks[hunk_idx].lines[line_idx]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorResolution {
    /// Still at its recorded line number with the same content.
    InPlace { file_idx: usize, hunk_idx: usize, line_idx: usize },
    /// Drifted, but the content was found nearby and re-anchored.
    Moved { file_idx: usize, hunk_idx: usize, line_idx: usize },
    /// Content changed, removed, or the file is absent/renamed; the
    /// thread renders against its captured context.
    Outdated,
}

/// FNV-1a hash of a diff line's text. Deterministic across runs (unlike
/// `DefaultHasher`), which matters because the value is persisted in the
/// anchor and compared after restarts.
pub fn content_hash(text: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Capture up to `radius` lines on each side of `line_idx` within `hunk`
/// (excluding the line itself) as fuzzy-match context.
pub fn capture_context(hunk: &Hunk, line_idx: usize, radius: usize) -> Vec<String> {
    let start = line_idx.saturating_sub(radius);
    let end = line_idx.saturating_add(radius).saturating_add(1).min(hunk.lines.len());
    (start..end).filter(|&i| i != line_idx).map(|i| hunk.lines[i].text.clone()).collect()
}

/// Re-resolve `anchor` against a fresh `files` scan.
pub fn resolve_anchor(anchor: &ReviewAnchor, files: &[FileHunks]) -> AnchorResolution {
    let Some(file_idx) = files.iter().position(|f| f.path == anchor.path) else {
        return AnchorResolution::Outdated;
    };
    let file = &files[file_idx];

    // In place: the exact recorded line number carrying the same content.
    for (hunk_idx, hunk) in file.hunks.iter().enumerate() {
        for (line_idx, line) in hunk.lines.iter().enumerate() {
            if side_line(line, anchor.side) == Some(anchor.line)
                && content_hash(&line.text) == anchor.content_hash
            {
                return AnchorResolution::InPlace { file_idx, hunk_idx, line_idx };
            }
        }
    }

    // Moved: same content within the drift window; context overlap wins,
    // then proximity. A clean shift has a single candidate, so ties only
    // arise for genuinely repeated content.
    let mut best: Option<(usize, usize, usize, i64)> = None;
    for (hunk_idx, hunk) in file.hunks.iter().enumerate() {
        for (line_idx, line) in hunk.lines.iter().enumerate() {
            let Some(ln) = side_line(line, anchor.side) else {
                continue;
            };
            let distance = i64::from(ln) - i64::from(anchor.line);
            if content_hash(&line.text) != anchor.content_hash || distance.abs() > REANCHOR_WINDOW {
                continue;
            }
            let overlap =
                context_overlap(&anchor.context, &capture_context(hunk, line_idx, CONTEXT_RADIUS));
            let better = match best {
                None => true,
                Some((_, _, best_overlap, best_distance)) => {
                    overlap > best_overlap
                        || (overlap == best_overlap && distance.abs() < best_distance.abs())
                }
            };
            if better {
                best = Some((hunk_idx, line_idx, overlap, distance));
            }
        }
    }
    match best {
        Some((hunk_idx, line_idx, _, _)) => {
            AnchorResolution::Moved { file_idx, hunk_idx, line_idx }
        }
        None => AnchorResolution::Outdated,
    }
}

fn side_line(line: &DiffLine, side: ReviewSide) -> Option<u32> {
    match side {
        ReviewSide::Old => line.old_line,
        ReviewSide::New => line.new_line,
    }
}

fn context_overlap(anchor_ctx: &[String], candidate_ctx: &[String]) -> usize {
    anchor_ctx.iter().filter(|line| candidate_ctx.contains(line)).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::git_diff::hunks::{DiffLineKind, FileStatus};

    /// A `new`-side line (added or context) at `new_line`.
    fn new_line(text: &str, n: u32) -> DiffLine {
        DiffLine {
            kind: DiffLineKind::Added,
            text: text.to_owned(),
            old_line: None,
            new_line: Some(n),
        }
    }

    /// An `old`-side line (removed) at `old_line`.
    fn old_line(text: &str, n: u32) -> DiffLine {
        DiffLine {
            kind: DiffLineKind::Removed,
            text: text.to_owned(),
            old_line: Some(n),
            new_line: None,
        }
    }

    fn file(path: &str, lines: Vec<DiffLine>) -> FileHunks {
        FileHunks {
            path: path.to_owned(),
            status: FileStatus::Modified,
            hunks: vec![Hunk { old_start: 1, old_count: 0, new_start: 1, new_count: 0, lines }],
        }
    }

    fn anchor(
        path: &str,
        side: ReviewSide,
        line: u32,
        text: &str,
        context: &[&str],
    ) -> ReviewAnchor {
        ReviewAnchor {
            path: path.to_owned(),
            side,
            line,
            content_hash: content_hash(text),
            context: context.iter().map(|s| (*s).to_owned()).collect(),
            base_ref: "main".to_owned(),
        }
    }

    #[test]
    fn content_hash_is_deterministic_and_discriminating() {
        assert_eq!(content_hash("let x = 1;"), content_hash("let x = 1;"));
        assert_ne!(content_hash("let x = 1;"), content_hash("let x = 2;"));
        // Whitespace-sensitive: indentation change is a content change.
        assert_ne!(content_hash("  foo()"), content_hash("    foo()"));
    }

    #[test]
    fn capture_context_excludes_center_and_clamps_at_edges() {
        let hunk = Hunk {
            old_start: 1,
            old_count: 0,
            new_start: 1,
            new_count: 0,
            lines: vec![new_line("a", 1), new_line("b", 2), new_line("c", 3), new_line("d", 4)],
        };
        // Middle: one before, two after (clamped by len).
        assert_eq!(
            capture_context(&hunk, 1, 2),
            vec!["a".to_owned(), "c".to_owned(), "d".to_owned()]
        );
        // First line: nothing before.
        assert_eq!(capture_context(&hunk, 0, 1), vec!["b".to_owned()]);
        // Last line: nothing after.
        assert_eq!(capture_context(&hunk, 3, 1), vec!["c".to_owned()]);
    }

    #[test]
    fn exact_line_and_content_is_in_place() {
        let files = vec![file(
            "src/x.rs",
            vec![new_line("fn a() {", 4), new_line("    body();", 5), new_line("}", 6)],
        )];
        let a = anchor("src/x.rs", ReviewSide::New, 5, "    body();", &["fn a() {", "}"]);
        assert_eq!(
            resolve_anchor(&a, &files),
            AnchorResolution::InPlace { file_idx: 0, hunk_idx: 0, line_idx: 1 },
        );
    }

    #[test]
    fn shifted_content_is_moved() {
        // The line drifted down two (a commit inserted lines above).
        let files = vec![file(
            "src/x.rs",
            vec![
                new_line("inserted_1", 5),
                new_line("inserted_2", 6),
                new_line("fn a() {", 7),
                new_line("    body();", 8),
                new_line("}", 9),
            ],
        )];
        // Recorded at line 6, content unchanged.
        let a = anchor("src/x.rs", ReviewSide::New, 6, "    body();", &["fn a() {", "}"]);
        assert_eq!(
            resolve_anchor(&a, &files),
            AnchorResolution::Moved { file_idx: 0, hunk_idx: 0, line_idx: 3 },
        );
    }

    #[test]
    fn changed_content_is_outdated() {
        // The anchored content no longer appears anywhere in the file.
        let files = vec![file(
            "src/x.rs",
            vec![new_line("fn a() {", 4), new_line("    renamed_body();", 5), new_line("}", 6)],
        )];
        let a = anchor("src/x.rs", ReviewSide::New, 5, "    body();", &["fn a() {", "}"]);
        assert_eq!(resolve_anchor(&a, &files), AnchorResolution::Outdated);
    }

    #[test]
    fn absent_or_renamed_file_is_outdated() {
        let files = vec![file("src/y.rs", vec![new_line("    body();", 5)])];
        let a = anchor("src/x.rs", ReviewSide::New, 5, "    body();", &[]);
        assert_eq!(resolve_anchor(&a, &files), AnchorResolution::Outdated);
    }

    #[test]
    fn old_side_anchor_resolves_against_old_line_numbers() {
        let files = vec![file(
            "src/x.rs",
            vec![old_line("removed_a", 3), old_line("removed_b", 4), new_line("added", 3)],
        )];
        let a = anchor("src/x.rs", ReviewSide::Old, 4, "removed_b", &["removed_a"]);
        assert_eq!(
            resolve_anchor(&a, &files),
            AnchorResolution::InPlace { file_idx: 0, hunk_idx: 0, line_idx: 1 },
        );
    }

    #[test]
    fn context_overlap_beats_proximity_for_repeated_content() {
        // Two identical `}` lines. The wrong one (line 12) is CLOSER to
        // the recorded line 14, but the right one (line 18) shares the
        // captured context, so context must win over distance.
        let files = vec![file(
            "src/x.rs",
            vec![
                new_line("    let x = compute();", 10),
                new_line("    return x;", 11),
                new_line("}", 12),
                new_line("", 13),
                new_line("fn helper() {", 14),
                new_line("    let y = other();", 15),
                new_line("    log(y);", 16),
                new_line("    return y;", 17),
                new_line("}", 18),
            ],
        )];
        let a = anchor(
            "src/x.rs",
            ReviewSide::New,
            14,
            "}",
            &["    let y = other();", "    log(y);", "    return y;"],
        );
        assert_eq!(
            resolve_anchor(&a, &files),
            AnchorResolution::Moved { file_idx: 0, hunk_idx: 0, line_idx: 8 },
            "context overlap must pick helper's brace (line 18), not the closer brace (line 12)",
        );
    }

    #[test]
    fn drift_beyond_window_is_outdated() {
        // Content moved 30 lines away (> REANCHOR_WINDOW).
        let files = vec![file("src/x.rs", vec![new_line("    body();", 40)])];
        let a = anchor("src/x.rs", ReviewSide::New, 10, "    body();", &[]);
        assert_eq!(resolve_anchor(&a, &files), AnchorResolution::Outdated);
    }
}
