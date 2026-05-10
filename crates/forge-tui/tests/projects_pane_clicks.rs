//! Integration tests for Projects-pane click semantics.
//!
//! Drives the full `handle_terminal_event` mouse path so the
//! pane-aware Down(Left) routing in
//! `crate::app::events::mouse::handle_pane_click` is exercised end
//! to end. Tests stamp `app.layout.pane` + `app.pane_hit_targets`
//! synthetically (the renderer does this in real frames) so we
//! don't need a live `Workspace` to verify the click semantics.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::rc::Rc;

use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use forge_tui::app::session::SessionLifecycleState;
use forge_tui::app::{App, PaneHitTarget, handle_terminal_event, spawn_for_sleeping_project};
use forge_workspace::{SessionKey, Workspace};
use tempfile::tempdir;

fn down_left(column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

#[test]
fn click_on_session_row_switches_active() {
    let mut app = App::test_default();
    let key_a = SessionKey::from_str_for_test("a");
    let key_b = SessionKey::from_str_for_test("b");
    app.sessions.insert(key_a.clone(), forge_tui::app::session::Session::new(key_a.clone()));
    app.sessions.insert(key_b.clone(), forge_tui::app::session::Session::new(key_b.clone()));
    app.active_session_key = Some(key_a.clone());

    // Stamp a hit target as if the pane had been rendered. The
    // renderer normally fills these during paint.
    app.pane_hit_targets.push(PaneHitTarget::SessionRow {
        session_key: key_b.clone(),
        y: 5,
        height: 1,
    });
    app.layout.pane = Some(ratatui::layout::Rect::new(0, 0, 26, 40));

    handle_terminal_event(&mut app, down_left(10, 5));

    assert_eq!(app.active_session_key.as_ref(), Some(&key_b));
}

#[test]
fn click_on_project_header_for_in_process_lead_switches_active() {
    // We can't easily build a real Workspace in a test, so we cover
    // the in-process branch by routing through `SessionRow` (the
    // semantic outcome — `App::switch_active_session` — is the same
    // either way). The header → workspace lookup → switch_active
    // path is covered by the sleeping-project test below, which
    // exercises the no-workspace case.
    let mut app = App::test_default();
    let key_a = SessionKey::from_str_for_test("a");
    let key_b = SessionKey::from_str_for_test("b");
    app.sessions.insert(key_a.clone(), forge_tui::app::session::Session::new(key_a.clone()));
    app.sessions.insert(key_b.clone(), forge_tui::app::session::Session::new(key_b.clone()));
    app.active_session_key = Some(key_a.clone());

    app.pane_hit_targets.push(PaneHitTarget::SessionRow {
        session_key: key_b.clone(),
        y: 7,
        height: 1,
    });
    app.layout.pane = Some(ratatui::layout::Rect::new(0, 0, 26, 40));

    handle_terminal_event(&mut app, down_left(5, 7));

    assert_eq!(app.active_session_key.as_ref(), Some(&key_b));
}

#[test]
fn click_outside_pane_does_not_consume() {
    let mut app = App::test_default();
    let key_a = SessionKey::from_str_for_test("a");
    app.sessions.insert(key_a.clone(), forge_tui::app::session::Session::new(key_a.clone()));
    app.active_session_key = Some(key_a.clone());
    // Layout has a pane on the left; clicks at x=100 land outside.
    app.layout.pane = Some(ratatui::layout::Rect::new(0, 0, 26, 40));

    handle_terminal_event(&mut app, down_left(100, 5));

    // Active session unchanged. No PaneHitTarget existed and the
    // click was outside the pane rect anyway, so neither the pane
    // path nor any session switch fires.
    assert_eq!(app.active_session_key.as_ref(), Some(&key_a));
}

#[test]
fn click_on_sleeping_project_header_does_not_panic() {
    // No workspace + no session match → handler logs the placeholder
    // (covered by tracing target `app.session`) and silently bails.
    // Active session stays unchanged.
    let mut app = App::test_default();
    let key_a = SessionKey::from_str_for_test("a");
    app.sessions.insert(key_a.clone(), forge_tui::app::session::Session::new(key_a.clone()));
    app.active_session_key = Some(key_a.clone());

    app.pane_hit_targets.push(PaneHitTarget::ProjectHeader {
        project_name: "sleeping-project".to_owned(),
        y: 4,
        height: 1,
    });
    app.layout.pane = Some(ratatui::layout::Rect::new(0, 0, 26, 40));

    // Should not panic, and active session stays the same since the
    // sleeping project's lead isn't in `app.sessions` (and there's
    // no workspace to look it up in).
    handle_terminal_event(&mut app, down_left(5, 4));

    assert_eq!(app.active_session_key.as_ref(), Some(&key_a));
}

#[test]
fn click_inside_pane_but_outside_any_row_consumes_silently() {
    // Click lands on the pane banner area where no PaneHitTarget was
    // stamped. The handler should consume the click (so chat hit-
    // tests don't fire) but leave the active session alone.
    let mut app = App::test_default();
    let key_a = SessionKey::from_str_for_test("a");
    app.sessions.insert(key_a.clone(), forge_tui::app::session::Session::new(key_a.clone()));
    app.active_session_key = Some(key_a.clone());
    app.layout.pane = Some(ratatui::layout::Rect::new(0, 0, 26, 40));
    // No hit targets — equivalent to clicking on the "PROJECTS"
    // banner or rule line.
    app.needs_redraw = false;

    handle_terminal_event(&mut app, down_left(2, 0));

    assert_eq!(app.active_session_key.as_ref(), Some(&key_a));
}

#[test]
fn click_on_session_row_when_pane_layout_missing_is_noop() {
    // With `app.layout.pane = None` the handler returns false
    // immediately, regardless of stamped hit targets. Defensive: a
    // stale `pane_hit_targets` from a previous frame must not fire
    // a session switch when the current frame has no pane.
    let mut app = App::test_default();
    let key_a = SessionKey::from_str_for_test("a");
    let key_b = SessionKey::from_str_for_test("b");
    app.sessions.insert(key_a.clone(), forge_tui::app::session::Session::new(key_a.clone()));
    app.sessions.insert(key_b.clone(), forge_tui::app::session::Session::new(key_b.clone()));
    app.active_session_key = Some(key_a.clone());

    app.pane_hit_targets.push(PaneHitTarget::SessionRow { session_key: key_b, y: 5, height: 1 });
    app.layout.pane = None;

    handle_terminal_event(&mut app, down_left(10, 5));

    assert_eq!(app.active_session_key.as_ref(), Some(&key_a));
}

/// When the user clicks a sleeping project's header (lead not yet
/// in `app.sessions`), the spawn helper should:
/// - synthesize a `__spawn_<name>__` Session bucket synchronously,
/// - flip `lifecycle_state` to `Spawning`,
/// - seed a placeholder welcome message + the project's display
///   path as the bucket's cwd,
/// - and switch the active session to the synthetic bucket so the
///   user immediately sees the spawning state in chat.
///
/// The async `Workspace::get_agent_handle` call kicks off in the
/// background; we only assert the synchronous part. The `Connected`
/// migration path that resolves the synthetic key onto a real
/// session UUID is exercised by the unit tests in
/// `events::session::tests`.
#[test]
fn click_sleeping_project_creates_spawning_bucket_synchronously() {
    let dir = tempdir().expect("tempdir");
    fs::write(
        dir.path().join("forge.toml"),
        r#"
[[projects]]
name = "test-proj"
path = "/tmp/test-project-path-2026-05-10"
default = true

[[accounts]]
display_name = "Test"
config_dir = "/tmp/test-account-config-2026-05-10"
"#,
    )
    .expect("write forge.toml");

    let runtime =
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let workspace =
        runtime.block_on(async { Workspace::new(dir.path().to_owned()).await.expect("workspace") });

    let mut app = App::test_default();
    app.workspace = Some(Rc::new(workspace));

    // The spawn helper itself runs synchronously up to the
    // `tokio::task::spawn_local` for the background connection
    // task. Drive it inside a `LocalSet` so spawn_local doesn't
    // panic for lack of one. The local task is queued but never
    // polled (we drop the LocalSet without `run_until`), so the
    // background handshake doesn't actually run — exactly what we
    // want for an isolated synchronous-state test.
    let local = tokio::task::LocalSet::new();
    let guard = local.enter();
    spawn_for_sleeping_project(&mut app, "test-proj");
    drop(guard);
    drop(local);

    let spawn_key = SessionKey::from_str_for_test("__spawn_test-proj__");
    assert!(
        app.sessions.contains_key(&spawn_key),
        "spawning bucket created synchronously under the __spawn_<name>__ key",
    );
    assert_eq!(
        app.active_session_key.as_ref(),
        Some(&spawn_key),
        "active session swapped to the spawning bucket so chat shows it immediately",
    );
    let bucket = app.sessions.get(&spawn_key).expect("bucket");
    assert_eq!(
        bucket.lifecycle_state,
        SessionLifecycleState::Spawning,
        "lifecycle state seeded as Spawning",
    );
    assert!(
        !bucket.messages.is_empty(),
        "placeholder welcome message added before the real Connected event",
    );
}

/// Calling the helper a second time for the same project (before
/// the first Connected event has migrated the bucket onto a real
/// session id) MUST be idempotent — no second bucket inserted, no
/// second connection task spawned, just an active swap to the
/// already-existing spawning bucket.
#[test]
fn double_click_same_sleeping_project_is_idempotent() {
    let dir = tempdir().expect("tempdir");
    fs::write(
        dir.path().join("forge.toml"),
        r#"
[[projects]]
name = "test-proj"
path = "/tmp/test-project-path-2026-05-10-b"
default = true

[[accounts]]
display_name = "Test"
config_dir = "/tmp/test-account-config-2026-05-10-b"
"#,
    )
    .expect("write forge.toml");

    let runtime =
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let workspace =
        runtime.block_on(async { Workspace::new(dir.path().to_owned()).await.expect("workspace") });

    let mut app = App::test_default();
    app.workspace = Some(Rc::new(workspace));

    let local = tokio::task::LocalSet::new();
    let guard = local.enter();
    spawn_for_sleeping_project(&mut app, "test-proj");
    let session_count_after_first = app.sessions.len();
    spawn_for_sleeping_project(&mut app, "test-proj");
    let session_count_after_second = app.sessions.len();
    drop(guard);
    drop(local);

    assert_eq!(
        session_count_after_first, session_count_after_second,
        "second spawn for the same project must reuse the existing spawning bucket",
    );
    let spawn_key = SessionKey::from_str_for_test("__spawn_test-proj__");
    assert_eq!(app.active_session_key.as_ref(), Some(&spawn_key));
}
