//! Shared fixtures for the diff overlay's tests: App/overlay builders
//! and review-thread seeds the per-module `mod tests` blocks reach via
//! `crate::app::diff_overlay::test_support::*`.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::keys::handle_key;
use super::mouse::open_input_for_key;
use super::state::DiffOverlayState;
use super::types::{ActiveCommentInput, CachedScan, DiffScope, HunkComment, LineKey, RailRowKey};
use crate::app::App;
use crate::app::input::InputState;
use crate::app::view::{ActiveView, set_active_view};
use crossterm::event::{KeyCode, KeyEvent};
use forge_primitives::git_diff::RepoGate;
use forge_primitives::review::{
    ReviewAnchor, ReviewAuthor, ReviewComment, ReviewSide, ReviewStatus,
};
use forge_workspace::env::git_diff::hunks::{
    CommitMeta, DiffLine, DiffLineKind, FileHunks, FileStatus, Hunk,
};
use forge_workspace::env::git_diff::resolver;

pub(crate) fn sample_state() -> DiffOverlayState {
    let mut state = DiffOverlayState::new(
        PathBuf::from("/tmp/repo"),
        "HEAD".to_owned(),
        vec![
            FileHunks {
                path: "a.rs".into(),
                status: FileStatus::Modified,
                hunks: vec![],
                oversize: false,
            },
            FileHunks {
                path: "b.rs".into(),
                status: FileStatus::Added,
                hunks: vec![],
                oversize: false,
            },
        ],
    );
    // Simulate what the renderer's tree pass would stash on
    // overlay state for the rail click handler. The two files
    // are top-level (no shared directory prefix) so the tree
    // is flat: banner/rule/blank then two file leaves.
    state.rail_keys = vec![
        RailRowKey::Banner,
        RailRowKey::Rule,
        RailRowKey::Blank,
        RailRowKey::File { file_idx: 0 },
        RailRowKey::File { file_idx: 1 },
    ];
    state
}

/// Diff view with a comment editor opened the way a line click does,
/// anchored on a real diff line so a save can resolve its anchor.
pub(crate) fn app_with_comment_editor() -> App {
    let mut app = App::test_default();
    let mut state = DiffOverlayState::new(
        PathBuf::from("/tmp/repo"),
        "main".to_owned(),
        vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])],
    );
    let _ = open_input_for_key(&mut state, LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 });
    app.diff_overlay = Some(state);
    crate::app::view::set_active_view(&mut app, ActiveView::Diff);
    app
}

/// Put the burst detector into the state mid-dictation produces:
/// three characters at machine speed.
pub(crate) fn start_dictation_burst(app: &mut App, base: std::time::Instant) {
    for (offset, ch) in [(0_u64, 'f'), (2, 'i'), (4, 'x')] {
        let _ = app.paste_burst.on_char(ch, base + Duration::from_millis(offset));
    }
    assert!(app.paste_burst.is_buffering(), "three machine-speed chars form a burst");
}

/// Type `token` one character at a time at human speed. Consecutive
/// test statements land microseconds apart, which the burst detector
/// correctly reads as a paste; clearing its timing reference between
/// keys is what "the user is typing" looks like to it.
pub(crate) fn type_text(app: &mut App, token: &str) {
    for ch in token.chars() {
        app.paste_burst.on_non_char_key(Instant::now());
        handle_key(app, KeyEvent::from(KeyCode::Char(ch)));
    }
}

/// Every text block of the last System message, for asserting on a
/// notice's wording rather than only its existence.
pub(crate) fn system_notice_text(app: &App) -> Option<String> {
    app.messages()
        .iter()
        .rev()
        .find(|m| matches!(m.role, crate::app::MessageRole::System(None)))
        .map(|m| {
            m.blocks
                .iter()
                .filter_map(|b| match b {
                    crate::app::MessageBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
}

pub(crate) fn target_snapshot(
    repo_gate: RepoGate,
    worktree_populated: bool,
    branch_ahead_populated: bool,
    default_branch: Option<&str>,
) -> forge_primitives::git_diff::GitDiffSnapshot {
    use forge_primitives::git_diff::{GitBranchAhead, GitDiffStats, LayerState};
    let worktree = if worktree_populated {
        LayerState::Populated(GitDiffStats::default())
    } else {
        LayerState::Clean
    };
    let branch_ahead = if branch_ahead_populated {
        LayerState::Populated(GitBranchAhead { commit_count: 1, stats: GitDiffStats::default() })
    } else {
        LayerState::Clean
    };
    forge_primitives::git_diff::GitDiffSnapshot {
        branch: forge_primitives::git::GitBranch::default(),
        default_branch: default_branch.map(str::to_owned),
        repo_gate,
        pushed_sha: None,
        worktree,
        branch_ahead,
        pr: None,
        closes: vec![],
        pr_fetched_at: None,
    }
}

pub(crate) fn app_with_target_snapshot(
    snapshot: Option<forge_primitives::git_diff::GitDiffSnapshot>,
) -> App {
    let mut app = App::test_default();
    let key = forge_workspace::SessionKey::from_session_id("diff-target-test");
    let mut session = crate::app::session::UiSession::new(key.clone());
    session.git_diff_snapshot = snapshot;
    app.sessions.insert(key.clone(), session);
    app.active_session_key = Some(key);
    app
}

pub(crate) fn commit_meta(sha: &str, subject: &str) -> CommitMeta {
    CommitMeta {
        sha: sha.to_owned(),
        short_sha: sha.to_owned(),
        subject: subject.to_owned(),
        body: String::new(),
    }
}

pub(crate) fn one_file(path: &str, status: FileStatus) -> FileHunks {
    FileHunks { path: path.to_owned(), status, hunks: vec![], oversize: false }
}

/// Three-commit branch with every commit's hunks pre-cached (so
/// navigation is synchronous). Commit 0 → a.rs, 1 → b.rs, 2 → c.rs.
pub(crate) fn commit_mode_state() -> DiffOverlayState {
    let c0 = vec![one_file("a.rs", FileStatus::Added)];
    let c1 = vec![one_file("b.rs", FileStatus::Modified)];
    let c2 = vec![one_file("c.rs", FileStatus::Modified)];
    let mut state =
        DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), c0.clone());
    state.commits = vec![
        commit_meta("aaa", "first"),
        commit_meta("bbb", "second"),
        commit_meta("ccc", "third"),
    ];
    state.branch = Some("feat".to_owned());
    state.scope = DiffScope::Commit(0);
    state.commit_cache = vec![
        Some(CachedScan { files: c0, scanner_ok: true }),
        Some(CachedScan { files: c1, scanner_ok: true }),
        Some(CachedScan { files: c2, scanner_ok: true }),
    ];
    state.recompute_comment_counts();
    state
}

pub(crate) fn cached_whole_diff() -> CachedScan {
    CachedScan { files: vec![one_file("x.rs", FileStatus::Modified)], scanner_ok: true }
}

pub(crate) fn diff_line(kind: DiffLineKind, old: Option<u32>, new: Option<u32>) -> DiffLine {
    DiffLine { kind, text: "x".to_owned(), old_line: old, new_line: new }
}

/// A full-context (wide) file: 30 new-file lines with additions at
/// lines 5 and 25, leaving a wide unchanged middle. The overlay
/// captures this at open; the default context narrows it to two hunks,
/// and expanding folds them back into one.
pub(crate) fn wide_file_with_two_changes() -> FileHunks {
    let mut lines = Vec::new();
    let mut old = 1u32;
    for new in 1..=30u32 {
        if new == 5 || new == 25 {
            lines.push(diff_line(DiffLineKind::Added, None, Some(new)));
        } else {
            lines.push(diff_line(DiffLineKind::Context, Some(old), Some(new)));
            old += 1;
        }
    }
    FileHunks {
        path: "a.rs".to_owned(),
        status: FileStatus::Modified,
        oversize: false,
        hunks: vec![Hunk { old_start: 1, old_count: old - 1, new_start: 1, new_count: 30, lines }],
    }
}

pub(crate) fn app_with_commit_overlay() -> App {
    let mut app = App::test_default();
    app.diff_overlay = Some(commit_mode_state());
    set_active_view(&mut app, ActiveView::Diff);
    app
}

pub(crate) fn overlay(app: &App) -> &DiffOverlayState {
    app.diff_overlay.as_ref().expect("overlay")
}

pub(crate) fn scope_thread(
    id: &str,
    commit: Option<&str>,
    updated_at: &str,
) -> forge_primitives::ReviewThread {
    forge_primitives::ReviewThread {
        id: id.to_owned(),
        anchor: ReviewAnchor {
            path: "src/x.rs".to_owned(),
            side: ReviewSide::New,
            line: 1,
            content_hash: 0,
            context: Vec::new(),
            base_ref: "main".to_owned(),
        },
        comments: Vec::new(),
        status: ReviewStatus::Open,
        created_at: "t0".to_owned(),
        updated_at: updated_at.to_owned(),
        commit: commit.map(str::to_owned),
    }
}

pub(crate) fn added_line(text: &str, new: u32) -> DiffLine {
    DiffLine {
        kind: DiffLineKind::Added,
        text: text.to_owned(),
        old_line: None,
        new_line: Some(new),
    }
}

pub(crate) fn single_hunk_file(path: &str, lines: Vec<DiffLine>) -> FileHunks {
    FileHunks {
        path: path.to_owned(),
        status: FileStatus::Modified,
        oversize: false,
        hunks: vec![forge_workspace::env::git_diff::hunks::Hunk {
            old_start: 1,
            old_count: 0,
            new_start: 1,
            new_count: 0,
            lines,
        }],
    }
}

/// A minimal Open review thread for tests that build a `HunkComment`
/// without caring about the thread's own contents.
/// A thread as a saved comment leaves it: one unfiled user turn, which
/// is what every production save path writes.
/// Each call mints a distinct id; a test wanting two cards on one
/// thread assigns `id` itself.
pub(crate) fn stock_thread() -> forge_primitives::ReviewThread {
    static NEXT_STOCK_ID: AtomicU64 = AtomicU64::new(1);
    forge_primitives::ReviewThread {
        id: format!("stock-{}", NEXT_STOCK_ID.fetch_add(1, Ordering::Relaxed)),
        anchor: ReviewAnchor {
            path: "src/x.rs".to_owned(),
            side: ReviewSide::New,
            line: 0,
            content_hash: 0,
            context: Vec::new(),
            base_ref: "main".to_owned(),
        },
        comments: vec![ReviewComment {
            author: ReviewAuthor::User,
            text: "stock note".to_owned(),
            at: String::new(),
            review_id: None,
        }],
        status: ReviewStatus::Open,
        created_at: String::new(),
        updated_at: String::new(),
        commit: None,
    }
}

/// A stock thread whose single `User` turn carries `text`, so a
/// per-turn reopen seeds the editor from `thread.comments[0]`.
pub(crate) fn user_thread(text: &str) -> forge_primitives::ReviewThread {
    let mut thread = stock_thread();
    thread.comments[0].text = text.to_owned();
    thread
}

/// A thread whose one user turn is already sealed into `review_id`, as
/// a submitted review leaves it.
pub(crate) fn filed_thread(review_id: &str) -> forge_primitives::ReviewThread {
    let mut thread = user_thread("filed note");
    thread.comments[0].review_id = Some(review_id.to_owned());
    thread
}

/// App wired with a workspace + redb + an active session under
/// project "forge", ready for review-thread persistence tests.
pub(crate) fn review_app() -> (App, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = App::test_default();
    let workspace = app.workspace.clone().expect("test workspace");
    workspace.install_db_for_test(
        forge_workspace::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
    );
    let key = forge_workspace::SessionKey::from_session_id("review-session");
    let mut session = crate::app::session::UiSession::new(key.clone());
    session.project = Some("forge".to_owned());
    session.cwd_raw = "/tmp/repo".into();
    app.sessions.insert(key.clone(), session);
    app.active_session_key = Some(key);
    (app, dir)
}

pub(crate) fn git(dir: &Path, args: &[&str]) {
    let out =
        std::process::Command::new("git").arg("-C").arg(dir).args(args).output().expect("git");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// A repo at `dir` on `branch`, with one commit so HEAD resolves.
/// `git init -b` needs git 2.28; CI runs 2.25, hence `symbolic-ref`.
pub(crate) fn init_repo(dir: &Path, branch: &str) {
    git(dir, &["init", "-q"]);
    git(dir, &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("README.md"), "hi\n").expect("write");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

pub(crate) fn with_editor(overlay: &mut DiffOverlayState, key: LineKey, text: &str) {
    let mut editor = InputState::new();
    editor.insert_str(text);
    overlay.active_input =
        Some(ActiveCommentInput { key, editor, prior_comment: None, edit_turn: None });
}

/// [`review_app`]-style workspace + redb, plus a live agent stub and a
/// session id wired through `set_session_id` so `dispatch_command`
/// reaches the stub. The Finish-review submit path then seals +
/// dispatches instead of holding on the no-agent guard.
pub(crate) fn review_app_with_agent()
-> (App, tokio::sync::mpsc::UnboundedReceiver<forge_primitives::AgentCommand>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = App::test_default();
    let workspace = app.workspace.clone().expect("test workspace");
    workspace.install_db_for_test(
        forge_workspace::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
    );
    let rx = app.install_testing_stub();
    app.set_session_id(Some(crate::agent::model::SessionId::new("review-session")));
    if let Some(key) = app.active_session_key.clone()
        && let Some(session) = app.sessions.get_mut(&key)
    {
        session.project = Some("forge".to_owned());
        session.cwd_raw = "/tmp/repo".into();
    }
    (app, rx, dir)
}

pub(crate) fn test_anchor() -> ReviewAnchor {
    ReviewAnchor {
        path: "a.rs".to_owned(),
        side: ReviewSide::New,
        line: 1,
        content_hash: 0,
        context: Vec::new(),
        base_ref: "main".to_owned(),
    }
}

pub(crate) fn agent_turn(text: &str) -> ReviewComment {
    ReviewComment {
        author: ReviewAuthor::Agent { label: "impl".to_owned() },
        text: text.to_owned(),
        at: String::new(),
        review_id: None,
    }
}

/// Build an overlay + persisted thread, then an empty editor over
/// `edit_turn`, ready to exercise the clear-a-turn save path.
pub(crate) fn clear_turn_setup(
    turns: Vec<ReviewComment>,
    edit_turn: usize,
) -> (App, std::sync::Arc<forge_workspace::Workspace>, tempfile::TempDir) {
    let (mut app, dir) = review_app();
    let ws = app.workspace.clone().expect("ws");
    let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
    let mut overlay = DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
    overlay.branch = Some("feat".to_owned());
    let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
    let mut thread = user_thread("seed");
    thread.id = "t-clear".to_owned();
    thread.comments = turns;
    ws.upsert_review_thread("forge", "feat", thread.clone());
    let prior = HunkComment {
        key,
        path: "src/x.rs".into(),
        line: 10,
        comment_text: thread.comments.first().map(|c| c.text.clone()).unwrap_or_default(),
        commit: None,
        thread,
        authored_this_session: true,
        anchor_note: None,
        persisted: true,
    };
    overlay.active_input = Some(ActiveCommentInput {
        key,
        editor: InputState::new(),
        prior_comment: Some(prior),
        edit_turn: Some(edit_turn),
    });
    app.diff_overlay = Some(overlay);
    (app, ws, dir)
}

/// A thread homed on a commit, whose content sits at a different line
/// number in the whole-branch diff than in the commit's own diff.
pub(crate) fn cross_numbered_thread() -> forge_primitives::ReviewThread {
    let mut thread = stock_thread();
    thread.id = "homed".to_owned();
    thread.commit = Some("aaa".to_owned());
    thread.anchor = ReviewAnchor {
        path: "src/x.rs".to_owned(),
        side: ReviewSide::New,
        // The commit's own diff numbers this line 41.
        line: 41,
        content_hash: resolver::anchor_hash("let a = 1;"),
        context: vec!["fn wrapper() {".to_owned(), "}".to_owned()],
        base_ref: "main".to_owned(),
    };
    thread
}

/// Whole-diff scan where the same content is numbered 5, with the
/// commit's own scan numbering it 41.
pub(crate) fn cross_numbered_overlay() -> DiffOverlayState {
    let whole = vec![single_hunk_file(
        "src/x.rs",
        vec![added_line("fn wrapper() {", 4), added_line("let a = 1;", 5), added_line("}", 6)],
    )];
    let commit = vec![single_hunk_file(
        "src/x.rs",
        vec![added_line("fn wrapper() {", 40), added_line("let a = 1;", 41), added_line("}", 42)],
    )];
    let mut overlay =
        DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), whole.clone());
    overlay.branch = Some("feat".to_owned());
    overlay.commits = vec![commit_meta("aaa", "first")];
    overlay.commit_cache = vec![Some(CachedScan { files: commit, scanner_ok: true })];
    overlay.whole_diff_cache = Some(CachedScan { files: whole, scanner_ok: true });
    overlay.scope = DiffScope::WholeDiff;
    overlay
}

pub(crate) fn thread_status(app: &App) -> ReviewStatus {
    app.diff_overlay.as_ref().expect("overlay").comments.first().expect("a comment").thread.status
}

/// A thread the worker answered, anchored on `let y = 1;` at line 10
/// so it re-resolves cleanly through `hydrate_threads`.
pub(crate) fn answered_thread(id: &str) -> forge_primitives::ReviewThread {
    let mut thread = user_thread("look here");
    thread.id = id.to_owned();
    thread.anchor.line = 10;
    thread.anchor.content_hash = resolver::content_hash("let y = 1;");
    thread.comments.push(ReviewComment {
        author: ReviewAuthor::Agent { label: "impl".to_owned() },
        text: "done".to_owned(),
        at: String::new(),
        review_id: None,
    });
    thread.status = ReviewStatus::Addressed;
    thread
}

/// Overlay over a one-line file, on branch `feat`, ready to hydrate
/// `answered_thread`'s anchor.
pub(crate) fn overlay_for_answered_threads() -> DiffOverlayState {
    let mut overlay = DiffOverlayState::new(
        PathBuf::from("/tmp/repo"),
        "main".to_owned(),
        vec![single_hunk_file("src/x.rs", vec![added_line("let y = 1;", 10)])],
    );
    overlay.branch = Some("feat".to_owned());
    overlay
}

pub(crate) fn waiting_count(app: &App) -> Option<usize> {
    app.active_session().and_then(|s| s.review_replies_waiting.as_ref()).map(|w| w.count)
}
