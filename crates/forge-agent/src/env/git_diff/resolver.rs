//! Re-anchoring resolver for durable review threads.
//!
//! A persisted [`ReviewAnchor`] stores a drift-robust location (path +
//! side + line + content hash + a few context lines). The diff is
//! re-scanned on every overlay open, so the anchor must be re-resolved
//! to a fresh positional index into the new [`FileHunks`].
//! [`resolve_anchor`] reports whether the line is still in place, moved,
//! or could not be placed at all.
//!
//! Relocation requires one unambiguous match: the line, plus at least
//! one recorded neighbour agreeing within [`CONTEXT_RADIUS`] on each
//! side the candidate has neighbours. Anything less is left where it was
//! and marked outdated, because a comment silently attached to the wrong
//! code costs far more than one the reviewer has to go looking for.
//!
//! A distance bound does not make that safer. Bounding how far a match
//! may be found still leaves several candidates inside the bound, and
//! choosing among them by proximity is exactly the guess uniqueness
//! refuses to make.

use std::collections::HashSet;

use forge_primitives::review::{ReviewAnchor, ReviewSide, is_doc_scope};

use super::content::FileContent;
use super::hunks::{DiffLine, FileHunks, Hunk};

/// Lines captured on each side of an anchored line for fuzzy re-anchor
/// disambiguation.
pub const CONTEXT_RADIUS: usize = 3;

/// Where a re-resolved anchor landed against a fresh diff scan. Indices
/// address `files[file_idx].hunks[hunk_idx].lines[line_idx]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorResolution {
    /// Still at its recorded line number with the same content.
    InPlace { file_idx: usize, hunk_idx: usize, line_idx: usize },
    /// One unambiguous match elsewhere. `from` is the line it left, which
    /// the thread reports so a relocation is never silent.
    Moved { file_idx: usize, hunk_idx: usize, line_idx: usize, from: u32 },
    /// Not relocated. The thread renders against its captured context,
    /// saying why.
    Outdated(OutdatedReason),
}

/// Why an anchor did not relocate. Attaching a comment to the wrong code
/// costs more than leaving it behind, so anything short of one certain
/// match lands here rather than being guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutdatedReason {
    /// The anchored code appears nowhere in the current diff, or its file
    /// is absent or renamed.
    Gone,
    /// Several locations match it equally well, so picking one would be a
    /// guess.
    Ambiguous { matches: usize },
}

/// FNV-1a hash of raw bytes: what both a line's text and a whole document
/// go through, so one implementation pins both.
///
/// Deterministic across runs (unlike `DefaultHasher`), which matters because
/// the value is persisted in an anchor and compared after restarts.
pub fn content_hash_bytes(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// [`content_hash_bytes`] over a diff line's text.
pub fn content_hash(text: &str) -> u64 {
    content_hash_bytes(text.as_bytes())
}

/// Capture up to `radius` lines on each side of `line_idx` within `hunk`
/// (excluding the line itself) as fuzzy-match context.
pub fn capture_context(hunk: &Hunk, line_idx: usize, radius: usize) -> Vec<String> {
    let start = line_idx.saturating_sub(radius);
    let end = line_idx.saturating_add(radius).saturating_add(1).min(hunk.lines.len());
    (start..end).filter(|&i| i != line_idx).map(|i| hunk.lines[i].text.clone()).collect()
}

/// Collapse every run of whitespace to one space and trim the ends.
fn normalise_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The hash to persist for a newly captured anchor. Whitespace-normalised
/// so a `cargo fmt` reflow reads as the same line.
pub fn anchor_hash(text: &str) -> u64 {
    content_hash(&normalise_ws(text))
}

/// Whether `text` is the line `anchor` was placed on. Anchors captured
/// before normalisation hold the raw hash, so both forms are accepted;
/// such an anchor keeps its old exact-match behaviour until it is
/// rewritten.
fn text_matches(anchor: &ReviewAnchor, text: &str) -> bool {
    anchor_hash(text) == anchor.content_hash || content_hash(text) == anchor.content_hash
}

/// Whether the code around `line_idx` still looks like where the comment
/// was left. Each side the candidate has neighbours on must carry one of
/// the anchor's recorded lines within [`CONTEXT_RADIUS`], in any order. A
/// line on its own matches far too easily - a closing brace, a short
/// call - so a neighbour has to agree. One implementation over any
/// line-addressable slice, so the hunk resolver and the seat-tree one
/// cannot drift.
fn neighbourhood_agrees<'a>(
    anchor: &ReviewAnchor,
    line_idx: usize,
    len: usize,
    line_at: impl Fn(usize) -> Option<&'a str>,
) -> bool {
    let recorded: HashSet<String> = anchor.context.iter().map(|l| normalise_ws(l)).collect();
    let agrees = |i: usize| line_at(i).is_some_and(|l| recorded.contains(&normalise_ws(l)));
    let before = (line_idx.saturating_sub(CONTEXT_RADIUS)..line_idx).any(agrees);
    let after = (line_idx.saturating_add(1)
        ..line_idx.saturating_add(CONTEXT_RADIUS).saturating_add(1).min(len))
        .any(agrees);
    // A hunk edge has no neighbour on that side, so nothing to agree with.
    let has_before = line_idx > 0;
    let has_after = line_idx.saturating_add(1) < len;
    (before || !has_before) && (after || !has_after) && (before || after)
}

fn neighbourhood_matches(anchor: &ReviewAnchor, hunk: &Hunk, line_idx: usize) -> bool {
    neighbourhood_agrees(anchor, line_idx, hunk.lines.len(), |i| {
        hunk.lines.get(i).map(|l| l.text.as_str())
    })
}

/// Re-resolve `anchor` against a fresh `files` scan.
/// `trust_line_number` says whether `files` is numbered the same way the
/// anchor was recorded in. Only then does the recorded line carry
/// information; from another scope's diff that number addresses whatever
/// happens to sit there, so the match has to be earned by content and
/// neighbours like any other.
pub fn resolve_anchor(
    anchor: &ReviewAnchor,
    files: &[FileHunks],
    trust_line_number: bool,
) -> AnchorResolution {
    let located: Vec<(&str, &[Hunk])> =
        files.iter().map(|file| (file.path.as_str(), file.hunks.as_slice())).collect();
    resolve_in(anchor, &located, trust_line_number)
}

/// Re-resolve `anchor` against the socket's bounded content read, whose
/// files carry hunks but not the overlay scan's own shape. Same
/// resolution as [`resolve_anchor`], one implementation.
pub fn resolve_anchor_in_content(
    anchor: &ReviewAnchor,
    files: &[FileContent],
    trust_line_number: bool,
) -> AnchorResolution {
    let located: Vec<(&str, &[Hunk])> =
        files.iter().map(|file| (file.path.as_str(), file.hunks.as_slice())).collect();
    resolve_in(anchor, &located, trust_line_number)
}

/// One anchor's landing in the terms a view draws it: no positional
/// indices into one scan's arrays, just the line the anchor sits on now
/// and, when it could not be placed, the resolver's own reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorPlacement {
    /// Placed, at `line` in the anchor's own side's numbering.
    /// `moved_from` is the recorded line when that is not where it sits.
    Placed { line: u32, moved_from: Option<u32> },
    /// Not placed, for [`OutdatedReason`]'s reason.
    Outdated(OutdatedReason),
}

/// [`resolve_anchor_in_content`], narrowed to what a view draws.
pub fn place_anchor_in_content(
    anchor: &ReviewAnchor,
    files: &[FileContent],
    trust_line_number: bool,
) -> AnchorPlacement {
    let line_at = |file_idx: usize, hunk_idx: usize, line_idx: usize| {
        files
            .get(file_idx)
            .and_then(|file| file.hunks.get(hunk_idx))
            .and_then(|hunk| hunk.lines.get(line_idx))
            .and_then(|line| side_line(line, anchor.side))
            // The resolver only lands on lines carrying the anchor's own
            // side, so this is its recorded line at worst.
            .unwrap_or(anchor.line)
    };
    match resolve_anchor_in_content(anchor, files, trust_line_number) {
        AnchorResolution::InPlace { file_idx, hunk_idx, line_idx } => AnchorPlacement::Placed {
            line: line_at(file_idx, hunk_idx, line_idx),
            moved_from: None,
        },
        AnchorResolution::Moved { file_idx, hunk_idx, line_idx, from } => AnchorPlacement::Placed {
            line: line_at(file_idx, hunk_idx, line_idx),
            moved_from: Some(from),
        },
        AnchorResolution::Outdated(reason) => AnchorPlacement::Outdated(reason),
    }
}

/// One seat-tree anchor placed against a file's whole current lines - the
/// files page's resolution. The scope's numbering IS the file's own (the
/// seat's tree is where the comment was written), so unlike a diff scope
/// whose numbering may belong to another base, the recorded line is
/// trusted: in place when it still carries the anchor's text, otherwise
/// re-found by content plus neighbours, and `Gone` / `Ambiguous` as the
/// diff resolver reports them.
pub fn place_anchor_in_lines(anchor: &ReviewAnchor, lines: &[String]) -> AnchorPlacement {
    let line_at = |i: usize| lines.get(i).map(String::as_str);
    if let Some(idx) = anchor.line.checked_sub(1).and_then(|i| usize::try_from(i).ok())
        && let Some(text) = line_at(idx)
        && text_matches(anchor, text)
    {
        return AnchorPlacement::Placed { line: anchor.line, moved_from: None };
    }
    let mut found: Vec<u32> = Vec::new();
    for (idx, text) in lines.iter().enumerate() {
        if text_matches(anchor, text) && neighbourhood_agrees(anchor, idx, lines.len(), line_at) {
            found.push(u32::try_from(idx + 1).unwrap_or(u32::MAX));
        }
    }
    match found[..] {
        [] => AnchorPlacement::Outdated(OutdatedReason::Gone),
        [line] => AnchorPlacement::Placed { line, moved_from: Some(anchor.line) },
        _ => AnchorPlacement::Outdated(OutdatedReason::Ambiguous { matches: found.len() }),
    }
}

fn resolve_in(
    anchor: &ReviewAnchor,
    files: &[(&str, &[Hunk])],
    trust_line_number: bool,
) -> AnchorResolution {
    // A doc scope is never a place in a diff, however its path collides
    // with a changed file: the viewer places those threads, not this. The
    // guard lives in the ONE implementation, so both entry points - the
    // overlay's scan and the socket's content read - refuse a doc anchor
    // alike.
    if is_doc_scope(&anchor.base_ref) {
        return AnchorResolution::Outdated(OutdatedReason::Gone);
    }
    let Some(file_idx) = files.iter().position(|(path, _)| *path == anchor.path) else {
        return AnchorResolution::Outdated(OutdatedReason::Gone);
    };
    let (_, hunks) = &files[file_idx];

    // In place: the recorded line number still carrying the same content.
    if trust_line_number {
        for (hunk_idx, hunk) in hunks.iter().enumerate() {
            for (line_idx, line) in hunk.lines.iter().enumerate() {
                if side_line(line, anchor.side) == Some(anchor.line)
                    && text_matches(anchor, &line.text)
                {
                    return AnchorResolution::InPlace { file_idx, hunk_idx, line_idx };
                }
            }
        }
    }

    let mut found = Vec::new();
    for (hunk_idx, hunk) in hunks.iter().enumerate() {
        for (line_idx, line) in hunk.lines.iter().enumerate() {
            if side_line(line, anchor.side).is_some()
                && text_matches(anchor, &line.text)
                && neighbourhood_matches(anchor, hunk, line_idx)
            {
                found.push((hunk_idx, line_idx));
            }
        }
    }
    match found[..] {
        [] => AnchorResolution::Outdated(OutdatedReason::Gone),
        [(hunk_idx, line_idx)] => {
            AnchorResolution::Moved { file_idx, hunk_idx, line_idx, from: anchor.line }
        }
        _ => AnchorResolution::Outdated(OutdatedReason::Ambiguous { matches: found.len() }),
    }
}

fn side_line(line: &DiffLine, side: ReviewSide) -> Option<u32> {
    match side {
        ReviewSide::Old => line.old_line,
        ReviewSide::New => line.new_line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::git_diff::hunks::{DiffLineKind, FileStatus};

    /// The byte hash is the same function the text hash is, so a document's
    /// hash and a line's hash share one implementation: text hashing is
    /// this over its UTF-8 bytes.
    #[test]
    fn the_byte_hash_matches_the_text_hash_over_its_bytes() {
        assert_eq!(content_hash_bytes(b"let x = 1;"), content_hash("let x = 1;"));
        assert_ne!(content_hash_bytes(b"a"), content_hash_bytes(b"b"));
    }

    /// **A doc-scoped anchor is never placed against a diff.** The scope
    /// spells a document, and the viewer places those threads, not the
    /// resolver: without the guard a document whose path coincides with a
    /// changed file could have its comments resolved onto code.
    #[test]
    fn a_doc_scoped_anchor_is_never_resolved() {
        let files = vec![file("docs/plan.md", vec![new_line("# Plan", 1), new_line("body", 2)])];
        let mut a = anchor("docs/plan.md", ReviewSide::New, 1, "# Plan", &[]);
        a.base_ref = forge_primitives::review::doc_scope_ref("/proj/docs/plan.md");

        assert_eq!(
            resolve_anchor(&a, &files, true),
            AnchorResolution::Outdated(OutdatedReason::Gone),
            "a document is not a place in the diff's numbering",
        );
    }

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
            oversize: false,
        }
    }

    fn content_file(path: &str, lines: Vec<DiffLine>) -> FileContent {
        FileContent {
            path: path.to_owned(),
            old_path: None,
            status: FileStatus::Modified,
            binary: false,
            submodule: false,
            truncated: false,
            hunks: vec![Hunk { old_start: 1, old_count: 0, new_start: 1, new_count: 0, lines }],
        }
    }

    /// The socket path resolves exactly as the overlay's own scan does -
    /// one resolution, two content shapes - and a file the content does
    /// not carry is the same Gone the overlay reports.
    #[test]
    fn the_content_read_resolves_like_the_overlay_scan() {
        let lines = vec![new_line("fn a() {", 4), new_line("    body();", 5), new_line("}", 6)];
        let overlay = vec![file("src/x.rs", lines.clone())];
        let content = vec![content_file("src/x.rs", lines)];
        let placed = anchor("src/x.rs", ReviewSide::New, 5, "    body();", &["fn a() {", "}"]);
        assert_eq!(
            resolve_anchor_in_content(&placed, &content, true),
            resolve_anchor(&placed, &overlay, true),
        );
        let absent = anchor("src/y.rs", ReviewSide::New, 5, "    body();", &[]);
        assert_eq!(
            resolve_anchor_in_content(&absent, &content, true),
            AnchorResolution::Outdated(OutdatedReason::Gone),
        );
    }

    /// The values the web client's own `anchorHash` is pinned against: the
    /// page computes this hash for the anchors it writes, and the two
    /// implementations must agree byte for byte or a thread written there
    /// never resolves. Whitespace collapsing and a non-ASCII
    /// byte are both exercised, because the two are where a port drifts.
    ///
    /// **The last two vectors are the collapse's edges**, where JS's `\s`
    /// and Rust's `split_whitespace` disagree: U+0085 (NEL) is Unicode
    /// White_Space and splits, U+FEFF (the BOM) is not and stays. The JS
    /// side names the class explicitly for exactly these two.
    #[test]
    fn the_hash_vectors_the_web_client_mirrors() {
        assert_eq!(anchor_hash("let x = 1;"), 12_051_066_714_322_805_831);
        assert_eq!(anchor_hash("  a  b  "), 16_591_147_884_985_137_298);
        assert_eq!(anchor_hash("fn caf\u{e9}() {}"), 14_060_598_129_659_224_412);
        assert_eq!(anchor_hash("a\u{85}b"), 16_591_147_884_985_137_298);
        assert_eq!(anchor_hash("a\u{feff}b"), 4_988_096_857_352_653_883);
    }

    /// The view-shaped placement names the line it landed on and the one
    /// it left - the two numbers a client draws, neither of which is an
    /// index into this scan.
    #[test]
    fn the_placement_names_the_line_it_moved_to() {
        let content = vec![content_file(
            "src/x.rs",
            vec![new_line("fn a() {", 8), new_line("    body();", 9), new_line("}", 10)],
        )];
        let moved = anchor("src/x.rs", ReviewSide::New, 5, "    body();", &["fn a() {", "}"]);
        assert_eq!(
            place_anchor_in_content(&moved, &content, false),
            AnchorPlacement::Placed { line: 9, moved_from: Some(5) },
        );
        let home = anchor("src/x.rs", ReviewSide::New, 9, "    body();", &["fn a() {", "}"]);
        assert_eq!(
            place_anchor_in_content(&home, &content, true),
            AnchorPlacement::Placed { line: 9, moved_from: None },
            "in place draws no note",
        );
    }

    /// A seat-tree anchor resolves against the file's whole current lines:
    /// the recorded number is the file's own numbering (the tree WAS the
    /// anchor's home when it was written), so the in-place shortcut
    /// applies, and a shifted line is re-found by content plus neighbours
    /// with the recorded line named as `moved_from`.
    #[test]
    fn a_tree_anchor_places_in_place_and_moved_against_file_lines() {
        let lines =
            |texts: &[&str]| -> Vec<String> { texts.iter().map(|t| (*t).to_owned()).collect() };
        let home = lines(&["fn f() {", "    let a = 1;", "    let b = 2;", "}", "// tail"]);
        let placed = anchor(
            "src/x.rs",
            ReviewSide::New,
            2,
            "    let a = 1;",
            &["fn f() {", "    let b = 2;"],
        );
        assert_eq!(
            place_anchor_in_lines(&placed, &home),
            AnchorPlacement::Placed { line: 2, moved_from: None },
        );

        // The same content two lines down, neighbours intact: moved, and
        // the recorded line is named.
        let shifted = lines(&[
            "// header",
            "// header 2",
            "fn f() {",
            "    let a = 1;",
            "    let b = 2;",
            "}",
            "// tail",
        ]);
        assert_eq!(
            place_anchor_in_lines(&placed, &shifted),
            AnchorPlacement::Placed { line: 4, moved_from: Some(2) },
        );
    }

    /// The reasons match the diff resolver's: content gone is `Gone`;
    /// content plus neighbours matching twice is `Ambiguous`, because
    /// picking one would be the guess uniqueness refuses to make.
    #[test]
    fn a_tree_anchor_gone_and_ambiguous_read_as_the_diff_resolvers_reasons() {
        let lines =
            |texts: &[&str]| -> Vec<String> { texts.iter().map(|t| (*t).to_owned()).collect() };
        let placed = anchor(
            "src/x.rs",
            ReviewSide::New,
            2,
            "    let a = 1;",
            &["fn f() {", "    let b = 2;"],
        );

        let gone = lines(&["fn f() {", "    let a = 9;", "    let b = 2;", "}"]);
        assert_eq!(
            place_anchor_in_lines(&placed, &gone),
            AnchorPlacement::Outdated(OutdatedReason::Gone),
        );

        // Both copies away from the recorded line, so the in-place
        // shortcut cannot win: content plus neighbours match twice.
        let twice = lines(&[
            "// header",
            "fn f() {",
            "    let a = 1;",
            "    let b = 2;",
            "}",
            "fn f() {",
            "    let a = 1;",
            "    let b = 2;",
            "}",
        ]);
        assert_eq!(
            place_anchor_in_lines(&placed, &twice),
            AnchorPlacement::Outdated(OutdatedReason::Ambiguous { matches: 2 }),
        );
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
            content_hash: anchor_hash(text),
            context: context.iter().map(|s| (*s).to_owned()).collect(),
            base_ref: "main".to_owned(),
        }
    }

    #[test]
    fn an_anchor_stored_before_normalisation_still_resolves() {
        // Rows written when the hash was raw keep exact-match behaviour
        // rather than failing to resolve at all.
        let files = vec![file(
            "src/x.rs",
            vec![new_line("fn a() {", 4), new_line("    body();", 5), new_line("}", 6)],
        )];
        let mut legacy = anchor("src/x.rs", ReviewSide::New, 5, "    body();", &["fn a() {", "}"]);
        legacy.content_hash = content_hash("    body();");
        assert_eq!(
            resolve_anchor(&legacy, &files, true),
            AnchorResolution::InPlace { file_idx: 0, hunk_idx: 0, line_idx: 1 },
            "a raw-hash anchor still resolves rather than reading as gone",
        );
    }

    #[test]
    fn content_hash_is_deterministic_and_discriminating() {
        assert_eq!(content_hash("let x = 1;"), content_hash("let x = 1;"));
        assert_ne!(content_hash("let x = 1;"), content_hash("let x = 2;"));
    }

    #[test]
    fn an_anchor_hash_ignores_whitespace_but_not_content() {
        assert_eq!(anchor_hash("  foo()"), anchor_hash("    foo()"), "reindented is the same line");
        assert_eq!(anchor_hash("a  +  b"), anchor_hash("a + b"), "and so is respaced");
        assert_eq!(anchor_hash("foo() "), anchor_hash("foo()"), "trailing space is not content");
        assert_ne!(anchor_hash("foo()"), anchor_hash("foo(x)"), "the code itself still counts");
        assert_ne!(anchor_hash("a b"), anchor_hash("ab"), "collapsing runs never joins words");
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
            resolve_anchor(&a, &files, true),
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
            resolve_anchor(&a, &files, true),
            AnchorResolution::Moved { file_idx: 0, hunk_idx: 0, line_idx: 3, from: 6 },
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
        assert_eq!(
            resolve_anchor(&a, &files, true),
            AnchorResolution::Outdated(OutdatedReason::Gone)
        );
    }

    #[test]
    fn absent_or_renamed_file_is_outdated() {
        let files = vec![file("src/y.rs", vec![new_line("    body();", 5)])];
        let a = anchor("src/x.rs", ReviewSide::New, 5, "    body();", &[]);
        assert_eq!(
            resolve_anchor(&a, &files, true),
            AnchorResolution::Outdated(OutdatedReason::Gone)
        );
    }

    #[test]
    fn resolves_in_a_later_file_and_later_hunk() {
        // file_idx 1: the target lives in the second file.
        let files = vec![
            file("a.rs", vec![new_line("other", 1)]),
            file("b.rs", vec![new_line("target", 5)]),
        ];
        let a = anchor("b.rs", ReviewSide::New, 5, "target", &[]);
        assert_eq!(
            resolve_anchor(&a, &files, true),
            AnchorResolution::InPlace { file_idx: 1, hunk_idx: 0, line_idx: 0 },
        );
        // hunk_idx 1: the target lives in the second hunk.
        let two_hunk = vec![FileHunks {
            path: "c.rs".to_owned(),
            status: FileStatus::Modified,
            hunks: vec![
                Hunk {
                    old_start: 1,
                    old_count: 0,
                    new_start: 1,
                    new_count: 0,
                    lines: vec![new_line("h0", 1)],
                },
                Hunk {
                    old_start: 10,
                    old_count: 0,
                    new_start: 10,
                    new_count: 0,
                    lines: vec![new_line("h1", 10)],
                },
            ],
            oversize: false,
        }];
        let b = anchor("c.rs", ReviewSide::New, 10, "h1", &[]);
        assert_eq!(
            resolve_anchor(&b, &two_hunk, true),
            AnchorResolution::InPlace { file_idx: 0, hunk_idx: 1, line_idx: 0 },
        );
    }

    #[test]
    fn old_side_anchor_resolves_against_old_line_numbers() {
        let files = vec![file(
            "src/x.rs",
            vec![old_line("removed_a", 3), old_line("removed_b", 4), new_line("added", 3)],
        )];
        let a = anchor("src/x.rs", ReviewSide::Old, 4, "removed_b", &["removed_a"]);
        assert_eq!(
            resolve_anchor(&a, &files, true),
            AnchorResolution::InPlace { file_idx: 0, hunk_idx: 0, line_idx: 1 },
        );
    }

    #[test]
    fn the_neighbourhood_picks_between_two_identical_lines() {
        // Two identical `}` lines. The wrong one (line 12) is closer to
        // the recorded line 14; only the right one (line 18) sits among
        // the recorded neighbours, so exactly one candidate matches.
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
            resolve_anchor(&a, &files, true),
            AnchorResolution::Moved { file_idx: 0, hunk_idx: 0, line_idx: 8, from: 14 },
            "only helper's brace has the recorded neighbours; the closer brace is not a candidate",
        );
    }

    #[test]
    fn a_reindented_line_is_the_same_anchor() {
        // cargo fmt reflows indentation on every commit in this repo. That
        // is the same code, and an exact match would strand the comment.
        let files = vec![file(
            "src/x.rs",
            vec![new_line("fn a() {", 4), new_line("        body();", 5), new_line("}", 6)],
        )];
        let a = anchor("src/x.rs", ReviewSide::New, 5, "    body();", &["fn a() {", "}"]);
        assert_eq!(
            resolve_anchor(&a, &files, true),
            AnchorResolution::InPlace { file_idx: 0, hunk_idx: 0, line_idx: 1 },
            "indentation is not identity",
        );
    }

    #[test]
    fn two_matching_locations_do_not_relocate() {
        // The same line with the same neighbours twice. Picking one would
        // be a guess, and a wrong guess attaches the comment to code it
        // was never about.
        let files = vec![file(
            "src/x.rs",
            vec![
                new_line("fn helper() {", 10),
                new_line("    log(x);", 11),
                new_line("}", 12),
                new_line("fn helper() {", 20),
                new_line("    log(x);", 21),
                new_line("}", 22),
            ],
        )];
        let a = anchor("src/x.rs", ReviewSide::New, 40, "    log(x);", &["fn helper() {", "}"]);
        assert_eq!(
            resolve_anchor(&a, &files, true),
            AnchorResolution::Outdated(OutdatedReason::Ambiguous { matches: 2 }),
            "two candidates is not a relocation, it is a coin toss",
        );
    }

    #[test]
    fn a_line_whose_neighbours_all_changed_does_not_relocate() {
        // A bare brace matches almost anywhere; the neighbourhood is what
        // identifies it.
        let files = vec![file(
            "src/x.rs",
            vec![new_line("fn other() {", 30), new_line("    different();", 31), new_line("}", 32)],
        )];
        let a = anchor(
            "src/x.rs",
            ReviewSide::New,
            12,
            "}",
            &["    let x = compute();", "    return x;"],
        );
        assert_eq!(
            resolve_anchor(&a, &files, true),
            AnchorResolution::Outdated(OutdatedReason::Gone),
            "the line alone is not enough to move a comment onto",
        );
    }

    #[test]
    fn a_match_far_from_its_old_line_still_relocates() {
        // A rebase moves code hundreds of lines. Distance is not evidence
        // of a different line once the neighbourhood agrees.
        let files = vec![file(
            "src/x.rs",
            vec![new_line("fn a() {", 500), new_line("    body();", 501), new_line("}", 502)],
        )];
        let a = anchor("src/x.rs", ReviewSide::New, 5, "    body();", &["fn a() {", "}"]);
        assert_eq!(
            resolve_anchor(&a, &files, true),
            AnchorResolution::Moved { file_idx: 0, hunk_idx: 0, line_idx: 1, from: 5 },
            "a unique match relocates however far it travelled, and names where it came from",
        );
    }

    #[test]
    fn a_line_number_from_another_numbering_is_not_evidence() {
        // A commit-scoped anchor counts lines against `sha^..sha`. In the
        // whole-branch diff that number addresses unrelated code, so the
        // in-place shortcut - which trusts the number and skips the
        // neighbour check - lands the comment inside a different
        // function. Braces and short lines collide constantly.
        let files = vec![file(
            "src/x.rs",
            vec![
                new_line("fn zzz() {", 6),
                new_line("}", 7),
                new_line("fn helper() {", 20),
                new_line("    log(x);", 21),
                new_line("}", 22),
            ],
        )];
        let a = anchor("src/x.rs", ReviewSide::New, 7, "}", &["fn helper() {", "    log(x);"]);
        assert_eq!(
            resolve_anchor(&a, &files, false),
            AnchorResolution::Moved { file_idx: 0, hunk_idx: 0, line_idx: 4, from: 7 },
            "the neighbours decide, not a line number counted in another space",
        );
    }

    #[test]
    fn a_line_number_from_the_threads_own_view_is_still_trusted() {
        // In its own numbering the recorded line means what it says, so
        // an unchanged line resolves without needing neighbours - which
        // is what keeps a one-line hunk working.
        let files = vec![file("src/x.rs", vec![new_line("    body();", 5)])];
        let a = anchor("src/x.rs", ReviewSide::New, 5, "    body();", &[]);
        assert_eq!(
            resolve_anchor(&a, &files, true),
            AnchorResolution::InPlace { file_idx: 0, hunk_idx: 0, line_idx: 0 },
        );
    }
}
