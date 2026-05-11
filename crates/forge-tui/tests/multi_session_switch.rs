//! Integration test for App's multi-session backend.
//!
//! Verifies that two concurrent sessions maintain isolated state
//! buckets and that `App::switch_active_session` correctly swaps
//! which bucket the renderer reads from.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_tui::app::App;
use forge_workspace::SessionKey;

#[test]
fn two_sessions_maintain_isolated_state() {
    let mut app = App::test_default();
    let key_a = SessionKey::from_str_for_test("session-a");
    let key_b = SessionKey::from_str_for_test("session-b");

    // Set up two sessions in the map.
    app.sessions.insert(key_a.clone(), forge_tui::app::session::Session::new(key_a.clone()));
    app.sessions.insert(key_b.clone(), forge_tui::app::session::Session::new(key_b.clone()));
    app.active_session_key = Some(key_a.clone());

    // Mutate B's bucket directly with something distinguishable.
    {
        let b = app.sessions.get_mut(&key_b).expect("b");
        b.cwd = "/path/to/project-b".to_string();
        b.files_accessed = 42;
    }

    // A's bucket is untouched.
    let a = app.sessions.get(&key_a).expect("a");
    assert!(a.cwd.is_empty(), "A's cwd should be untouched, got: {}", a.cwd);
    assert_eq!(a.files_accessed, 0);

    // Renderer reads A.
    assert_eq!(app.active_session_key.as_ref(), Some(&key_a));
    assert_eq!(app.cwd(), "");

    // Reset needs_redraw so the post-switch assertion is meaningful
    // (test_default seeds it `true`).
    app.needs_redraw = false;

    // Switch active to B.
    app.switch_active_session(key_b.clone());
    assert_eq!(app.active_session_key.as_ref(), Some(&key_b));
    assert_eq!(app.cwd(), "/path/to/project-b");
    assert_eq!(app.files_accessed(), 42);
    assert!(app.needs_redraw, "switch should set needs_redraw");
}

#[test]
fn switch_to_same_session_is_noop() {
    let mut app = App::test_default();
    let key = SessionKey::from_str_for_test("same");
    app.sessions.insert(key.clone(), forge_tui::app::session::Session::new(key.clone()));
    app.active_session_key = Some(key.clone());
    app.needs_redraw = false;

    app.switch_active_session(key);
    assert!(!app.needs_redraw, "no-op should not set needs_redraw");
}

#[test]
fn switch_to_unknown_key_is_noop() {
    let mut app = App::test_default();
    let known = SessionKey::from_str_for_test("known");
    let unknown = SessionKey::from_str_for_test("unknown");
    app.sessions.insert(known.clone(), forge_tui::app::session::Session::new(known.clone()));
    app.active_session_key = Some(known.clone());
    app.needs_redraw = false;

    app.switch_active_session(unknown.clone());
    assert_eq!(app.active_session_key.as_ref(), Some(&known));
    assert!(!app.needs_redraw);
    assert!(!app.sessions.contains_key(&unknown), "unknown key must not be inserted");
}

/// Switching A → B → A must restore A's bucket exactly. Background
/// sessions accumulate state silently while another session is
/// rendered, and switching back must surface that state on the next
/// paint.
#[test]
fn switch_round_trip_preserves_state() {
    let mut app = App::test_default();
    let key_a = SessionKey::from_str_for_test("session-a");
    let key_b = SessionKey::from_str_for_test("session-b");
    app.sessions.insert(key_a.clone(), forge_tui::app::session::Session::new(key_a.clone()));
    app.sessions.insert(key_b.clone(), forge_tui::app::session::Session::new(key_b.clone()));
    app.active_session_key = Some(key_a.clone());

    // Mutate A's bucket.
    {
        let a = app.sessions.get_mut(&key_a).expect("a");
        a.cwd = "/from/a".to_string();
        a.files_accessed = 5;
    }

    // Mutate B's bucket.
    {
        let b = app.sessions.get_mut(&key_b).expect("b");
        b.cwd = "/from/b".to_string();
    }

    // A → B: B's bucket is now rendered.
    app.switch_active_session(key_b.clone());
    assert_eq!(app.cwd(), "/from/b");

    // B → A: A's bucket survives the round trip.
    app.switch_active_session(key_a);
    assert_eq!(app.cwd(), "/from/a", "A's cwd survives the round trip");
    assert_eq!(app.files_accessed(), 5, "A's files_accessed survives the round trip");
}
