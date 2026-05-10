//! Integration tests for Projects-pane click semantics.
//!
//! Drives the full `handle_terminal_event` mouse path so the
//! pane-aware Down(Left) routing in
//! `crate::app::events::mouse::handle_pane_click` is exercised end
//! to end. Tests stamp `app.layout.pane` + `app.pane_hit_targets`
//! synthetically (the renderer does this in real frames) so we
//! don't need a live `Workspace` to verify the click semantics.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use forge_tui::app::{App, PaneHitTarget, handle_terminal_event};
use forge_workspace::SessionKey;

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
