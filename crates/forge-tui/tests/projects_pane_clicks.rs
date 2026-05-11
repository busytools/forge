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
use std::sync::Arc;

use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use forge_tui::agent::events::ClientEvent;
use forge_tui::agent::model;
use forge_tui::app::session::SessionLifecycleState;
use forge_tui::app::{
    App, PaneHitTarget, handle_client_event, handle_terminal_event, spawn_for_sleeping_project,
};
use forge_workspace::{SessionKey, Workspace};
use tempfile::tempdir;

fn stub_conn() -> Arc<forge_agent::AgentHandle> {
    let (handle, _rx) = forge_agent::Agent::testing_stub();
    Arc::new(handle)
}

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
    // Cover the `switch_to_project_lead` workspace-lookup path: the
    // pane stamps a `ProjectHeader` hit target, the click handler
    // resolves the project's lead session via the live workspace,
    // and (when the lead is in `app.sessions`) hands off to
    // `switch_active_session`.
    //
    // To trigger that path we need a Workspace whose `list_projects`
    // returns a project with at least one session whose key matches
    // an in-process bucket. Build a `forge.toml` for "test-proj"
    // pointed at a tempdir, then plant a minimal session jsonl file
    // under `<config_dir>/projects/<sanitized>/<uuid>.jsonl` so
    // `Workspace::new`'s catalog scan picks it up. Insert a
    // `Session` keyed by that same UUID into `app.sessions`.
    let config_dir = tempdir().expect("config_dir tempdir");
    let project_dir = tempdir().expect("project tempdir");
    fs::write(
        config_dir.path().join("forge.toml"),
        format!(
            r#"
[[projects]]
name = "test-proj"
path = "{}"
default = true

[[accounts]]
display_name = "Test"
config_dir = "{}"
"#,
            project_dir.path().to_string_lossy(),
            project_dir.path().to_string_lossy(),
        ),
    )
    .expect("write forge.toml");

    // Compute the catalog directory the way Workspace expects:
    // `<config_dir>/projects/<sanitised_canonicalised_path>/`.
    let project_path_str = project_dir.path().to_string_lossy().to_string();
    let project_key =
        forge_agent::userdata::catalog::scan::project_key_for_directory(Some(&project_path_str));
    let catalog_dir = config_dir.path().join("projects").join(&project_key);
    fs::create_dir_all(&catalog_dir).expect("create catalog dir");

    // Plant a minimal session transcript. The catalog scanner only
    // needs a UUID-named .jsonl file with a parseable summary
    // hint — a single JSON line carrying `lastPrompt` satisfies the
    // summary fallback chain.
    let lead_uuid = "12345678-1234-4234-8234-123456789abc";
    let session_path = catalog_dir.join(format!("{lead_uuid}.jsonl"));
    fs::write(
        &session_path,
        format!(
            "{{\"type\":\"user\",\"sessionId\":\"{lead_uuid}\",\"cwd\":\"{}\",\"message\":{{\"content\":\"hi\"}},\"lastPrompt\":\"hi\",\"timestamp\":\"2026-05-10T12:00:00Z\"}}\n",
            project_dir.path().to_string_lossy(),
        ),
    )
    .expect("write session file");

    let runtime =
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let workspace = runtime
        .block_on(async { Workspace::new(config_dir.path().to_owned()).await.expect("workspace") });

    // Sanity-check that the catalog scan actually found the session
    // — without this the test below would silently fall back to the
    // sleeping-project spawn path and miss the workspace-lookup
    // branch we're trying to cover.
    let projects = workspace.list_projects();
    let project_view = projects.iter().find(|p| p.name == "test-proj").expect("test-proj surfaces");
    assert!(
        !project_view.sessions.is_empty(),
        "catalog scan must surface the planted session for the workspace-lookup branch"
    );
    let lead_key = project_view.sessions[0].session.clone();

    let mut app = App::test_default();
    app.workspace = Some(Rc::new(workspace));

    // Plant the lead session as an in-process bucket so the click
    // handler's "lead is in app.sessions" branch fires.
    app.sessions.insert(lead_key.clone(), forge_tui::app::session::Session::new(lead_key.clone()));
    let key_a = SessionKey::from_str_for_test("a");
    app.sessions.insert(key_a.clone(), forge_tui::app::session::Session::new(key_a.clone()));
    app.active_session_key = Some(key_a);

    // Stamp a ProjectHeader hit target keyed by the project's
    // canonicalised key (matches the renderer's stamp shape).
    app.pane_hit_targets.push(PaneHitTarget::ProjectHeader {
        project_name: project_view.key.as_str().to_owned(),
        y: 4,
        height: 1,
    });
    app.layout.pane = Some(ratatui::layout::Rect::new(0, 0, 26, 40));

    handle_terminal_event(&mut app, down_left(5, 4));

    assert_eq!(
        app.active_session_key.as_ref(),
        Some(&lead_key),
        "ProjectHeader click should switch active to the lead session via the workspace lookup",
    );
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

#[test]
fn click_top_bar_icon_toggles_overlay() {
    let mut app = App::test_default();
    let key_a = SessionKey::from_str_for_test("a");
    app.sessions.insert(key_a.clone(), forge_tui::app::session::Session::new(key_a.clone()));
    app.active_session_key = Some(key_a);

    // Stamp a top-bar icon target — width 1 col at the origin.
    app.pane_hit_targets.push(PaneHitTarget::TopBarIcon { y: 0, height: 1, x_start: 0, x_end: 1 });
    // Layout has no inline pane (Narrow tier semantics): top bar
    // sits above the body, no `pane`.
    app.layout.top_bar = Some(ratatui::layout::Rect::new(0, 0, 100, 1));
    app.layout.pane = None;

    assert!(!app.projects_pane_overlay_open);
    handle_terminal_event(&mut app, down_left(0, 0));
    assert!(app.projects_pane_overlay_open, "first click opens overlay");

    // Re-stamp before the second click — render-time stamps are
    // cleared / refilled each frame in production. The test
    // simulates a second frame by stamping again.
    app.pane_hit_targets.push(PaneHitTarget::TopBarIcon { y: 0, height: 1, x_start: 0, x_end: 1 });
    handle_terminal_event(&mut app, down_left(0, 0));
    assert!(!app.projects_pane_overlay_open, "second click closes overlay");
}

#[test]
fn click_outside_top_bar_icon_x_range_does_not_toggle() {
    // The icon sits at column 0 only. Clicking at column 5 (the
    // active-context label area) must NOT flip the overlay.
    let mut app = App::test_default();
    let key_a = SessionKey::from_str_for_test("a");
    app.sessions.insert(key_a.clone(), forge_tui::app::session::Session::new(key_a.clone()));
    app.active_session_key = Some(key_a);
    app.pane_hit_targets.push(PaneHitTarget::TopBarIcon { y: 0, height: 1, x_start: 0, x_end: 1 });
    app.layout.top_bar = Some(ratatui::layout::Rect::new(0, 0, 100, 1));
    app.layout.pane = None;

    handle_terminal_event(&mut app, down_left(5, 0));
    assert!(
        !app.projects_pane_overlay_open,
        "click outside the icon's x-range must not flip the overlay"
    );
}

#[test]
fn click_overlay_close_glyph_dismisses_without_switching() {
    let mut app = App::test_default();
    let key_a = SessionKey::from_str_for_test("a");
    app.sessions.insert(key_a.clone(), forge_tui::app::session::Session::new(key_a.clone()));
    app.active_session_key = Some(key_a.clone());
    app.projects_pane_overlay_open = true;
    // ✕ glyph stamped at the right edge of a 100-col overlay.
    app.pane_hit_targets.push(PaneHitTarget::OverlayClose {
        y: 0,
        height: 1,
        x_start: 99,
        x_end: 100,
    });
    app.layout.top_bar = Some(ratatui::layout::Rect::new(0, 0, 100, 1));
    app.layout.pane = None;

    handle_terminal_event(&mut app, down_left(99, 0));

    assert!(!app.projects_pane_overlay_open, "overlay closed");
    assert_eq!(
        app.active_session_key.as_ref(),
        Some(&key_a),
        "active session unchanged when ✕ is clicked"
    );
}

#[test]
fn click_session_row_in_overlay_switches_and_closes() {
    let mut app = App::test_default();
    let key_a = SessionKey::from_str_for_test("a");
    let key_b = SessionKey::from_str_for_test("b");
    app.sessions.insert(key_a.clone(), forge_tui::app::session::Session::new(key_a.clone()));
    app.sessions.insert(key_b.clone(), forge_tui::app::session::Session::new(key_b.clone()));
    app.active_session_key = Some(key_a);
    app.projects_pane_overlay_open = true;
    app.pane_hit_targets.push(PaneHitTarget::SessionRow {
        session_key: key_b.clone(),
        y: 5,
        height: 1,
    });
    app.layout.top_bar = Some(ratatui::layout::Rect::new(0, 0, 100, 1));
    app.layout.pane = None;

    handle_terminal_event(&mut app, down_left(10, 5));

    assert_eq!(app.active_session_key.as_ref(), Some(&key_b), "switched to B");
    assert!(!app.projects_pane_overlay_open, "overlay closed after row click");
}

#[test]
fn esc_closes_overlay_without_cancelling_turn() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = App::test_default();
    let key_a = SessionKey::from_str_for_test("a");
    app.sessions.insert(key_a.clone(), forge_tui::app::session::Session::new(key_a.clone()));
    app.active_session_key = Some(key_a);
    app.projects_pane_overlay_open = true;

    handle_terminal_event(&mut app, Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));

    assert!(!app.projects_pane_overlay_open, "Esc closes overlay first");
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

/// Helper used by the audit-fix tests below: build a stock
/// `Connected` event with the fields the migration handler reads.
fn connected_event_for(
    session_id: &str,
    cwd: &str,
    pre_connect_key: Option<SessionKey>,
) -> ClientEvent {
    ClientEvent::Connected {
        session_id: model::SessionId::new(session_id.to_owned()),
        cwd: cwd.to_owned(),
        current_model: model::CurrentModel::new("model", "model", "model").authoritative(true),
        available_models: Vec::new(),
        mode: None,
        history_updates: Vec::new(),
        pre_connect_key,
        conn: stub_conn(),
    }
}

/// C1 regression: a sleeping-project spawn failure must NOT kill
/// forge-tui. The spawn task wires `is_fatal_on_failure: false`,
/// so the failure surfaces inline as `ConnectionFailed` only —
/// `should_quit` stays false and the bucket lands back in
/// `Sleeping`. (Pre-fix: the unconditional `FatalError` send in
/// `emit_connection_failed` killed the app on every sleeping-spawn
/// failure.)
#[test]
fn spawn_failure_does_not_kill_forge_tui() {
    let mut app = App::test_default();
    let key_existing = SessionKey::from_str_for_test("existing-session");
    app.sessions
        .insert(key_existing.clone(), forge_tui::app::session::Session::new(key_existing.clone()));
    app.active_session_key = Some(key_existing.clone());

    // Synthesize a spawning bucket as if `spawn_for_sleeping_project`
    // had just kicked off but the bridge handshake then failed.
    let spawn_key = SessionKey::from_str_for_test("__spawn_failing-proj__");
    let mut bucket = forge_tui::app::session::Session::new(spawn_key.clone());
    bucket.lifecycle_state = SessionLifecycleState::Spawning;
    app.sessions.insert(spawn_key.clone(), bucket);
    // User has already switched away from the spawning bucket.
    // The failure should land on the spawn bucket without yanking
    // the active session.
    assert_eq!(app.active_session_key.as_ref(), Some(&key_existing));

    handle_client_event(
        &mut app,
        ClientEvent::ConnectionFailed {
            session_key: spawn_key.clone(),
            message: "workspace.get_agent_handle failed: simulated".to_owned(),
        },
    );

    // Without the C1 fix, the spawn task would have followed up with
    // `FatalError`, flipping `should_quit`. With the fix, the spawn
    // path's `is_fatal_on_failure: false` means only the
    // `ConnectionFailed` event is dispatched.
    assert!(!app.should_quit, "sleeping-project spawn failure must NOT kill forge-tui");
    assert!(app.exit_error.is_none(), "no fatal error should be set");
    let migrated = app.sessions.get(&spawn_key).expect("spawn bucket present");
    assert_eq!(
        migrated.lifecycle_state,
        SessionLifecycleState::Sleeping,
        "failed spawn lands the bucket back in Sleeping",
    );
}

/// C2 regression: rapid clicks on different sleeping projects must
/// each migrate ONLY their own bucket. Without the fix, the
/// migration heuristic ("any synthetic bucket that's currently
/// active") could pick up B's spawn bucket when A's `Connected`
/// arrived, scrambling cross-project data.
#[test]
fn rapid_clicks_on_different_sleeping_projects_each_get_correct_bucket() {
    let mut app = App::test_default();
    // Strip the test_default's pre-Connect bucket so the fixture is
    // explicit about the synthetic buckets in play.
    app.sessions.clear();

    // Click A: synthesize the `__spawn_A__` bucket.
    let spawn_a = SessionKey::from_str_for_test("__spawn_A__");
    let mut bucket_a = forge_tui::app::session::Session::new(spawn_a.clone());
    bucket_a.lifecycle_state = SessionLifecycleState::Spawning;
    bucket_a.cwd = "/projects/A".to_owned();
    bucket_a.cwd_raw = "/projects/A".to_owned();
    app.sessions.insert(spawn_a.clone(), bucket_a);

    // Click B: synthesize the `__spawn_B__` bucket; B is the active
    // session at the moment A's Connected fires.
    let spawn_b = SessionKey::from_str_for_test("__spawn_B__");
    let mut bucket_b = forge_tui::app::session::Session::new(spawn_b.clone());
    bucket_b.lifecycle_state = SessionLifecycleState::Spawning;
    bucket_b.cwd = "/projects/B".to_owned();
    bucket_b.cwd_raw = "/projects/B".to_owned();
    app.sessions.insert(spawn_b.clone(), bucket_b);
    app.active_session_key = Some(spawn_b.clone());

    // A's Connected lands. With the fix it carries
    // `pre_connect_key: Some(spawn_a)`, so the handler migrates A's
    // bucket onto A's UUID and leaves B alone.
    handle_client_event(
        &mut app,
        connected_event_for("uuid-A", "/projects/A", Some(spawn_a.clone())),
    );

    let real_a = SessionKey::from_session_id("uuid-A".to_owned());
    assert!(
        !app.sessions.contains_key(&spawn_a),
        "A's synthetic bucket migrated onto its real UUID",
    );
    assert!(app.sessions.contains_key(&real_a), "A's real-key bucket present");
    let migrated_a = app.sessions.get(&real_a).expect("A's bucket");
    assert_eq!(
        migrated_a.cwd_raw, "/projects/A",
        "A's bucket preserved its own cwd through the migration",
    );

    // B's bucket is untouched.
    assert!(
        app.sessions.contains_key(&spawn_b),
        "B's synthetic bucket must NOT have been migrated by A's Connected",
    );
    let untouched_b = app.sessions.get(&spawn_b).expect("B's bucket");
    assert_eq!(untouched_b.cwd_raw, "/projects/B", "B's bucket cwd preserved");
    assert_eq!(
        untouched_b.lifecycle_state,
        SessionLifecycleState::Spawning,
        "B's lifecycle still Spawning — its own Connected hasn't fired",
    );
}

/// C3 regression: when the user switches to a different session
/// during a sleeping-project spawn, that spawn's `Connected` must
/// NOT yank `active_session_key` away from the user's deliberate
/// pick.
#[test]
fn connected_for_background_spawn_does_not_hijack_active() {
    let mut app = App::test_default();
    app.sessions.clear();

    let spawn_a = SessionKey::from_str_for_test("__spawn_A__");
    let mut bucket_a = forge_tui::app::session::Session::new(spawn_a.clone());
    bucket_a.lifecycle_state = SessionLifecycleState::Spawning;
    bucket_a.cwd = "/projects/A".to_owned();
    bucket_a.cwd_raw = "/projects/A".to_owned();
    app.sessions.insert(spawn_a.clone(), bucket_a);

    // User had clicked the sleeping project A, then deliberately
    // switched to a known session X in the meantime.
    let key_x = SessionKey::from_str_for_test("known-session-X");
    let mut session_x = forge_tui::app::session::Session::new(key_x.clone());
    session_x.session_id = Some(model::SessionId::new("known-session-X"));
    app.sessions.insert(key_x.clone(), session_x);
    app.active_session_key = Some(key_x.clone());

    // A's Connected arrives.
    handle_client_event(
        &mut app,
        connected_event_for("uuid-A", "/projects/A", Some(spawn_a.clone())),
    );

    // The migration happened but did NOT yank the active session.
    let real_a = SessionKey::from_session_id("uuid-A".to_owned());
    assert!(app.sessions.contains_key(&real_a), "A's real-key bucket exists post-migration");
    assert_eq!(
        app.active_session_key.as_ref(),
        Some(&key_x),
        "active session must remain on X — the user's deliberate pick",
    );
}

/// I2 + I6 regression: handle_resize must clear the projects-pane
/// overlay flag and stale layout state. Without this, opening the
/// overlay at Narrow tier and resizing to Wide leaves the flag set
/// — Esc and chat clicks then route into stale overlay handlers.
#[test]
fn overlay_flag_cleared_on_resize() {
    let mut app = App::test_default();
    app.projects_pane_overlay_open = true;
    app.pane_hit_targets.push(PaneHitTarget::SessionRow {
        session_key: SessionKey::from_str_for_test("a"),
        y: 5,
        height: 1,
    });
    app.layout.pane = Some(ratatui::layout::Rect::new(0, 0, 26, 40));

    handle_terminal_event(&mut app, Event::Resize(200, 60));

    assert!(!app.projects_pane_overlay_open, "resize must clear overlay-open flag");
    assert!(app.pane_hit_targets.is_empty(), "resize must clear hit targets");
    assert!(
        app.layout.pane.is_none(),
        "resize must reset layout cache (pane rect cleared until next render)",
    );
}

/// I1 regression: a successful Connected migration leaves the
/// bucket's `lifecycle_state` at `Idle` rather than the stale
/// `Spawning` it had during the handshake. Without this, the
/// Projects pane drilldown spinner stays spinning forever.
#[test]
fn lifecycle_state_idle_after_connected() {
    let mut app = App::test_default();
    app.sessions.clear();

    let spawn_key = SessionKey::from_str_for_test("__spawn_proj__");
    let mut bucket = forge_tui::app::session::Session::new(spawn_key.clone());
    bucket.lifecycle_state = SessionLifecycleState::Spawning;
    bucket.cwd = "/projects/proj".to_owned();
    bucket.cwd_raw = "/projects/proj".to_owned();
    app.sessions.insert(spawn_key.clone(), bucket);
    app.active_session_key = Some(spawn_key.clone());

    handle_client_event(
        &mut app,
        connected_event_for("uuid-proj", "/projects/proj", Some(spawn_key.clone())),
    );

    let real = SessionKey::from_session_id("uuid-proj".to_owned());
    let migrated = app.sessions.get(&real).expect("real-key bucket");
    assert_eq!(
        migrated.lifecycle_state,
        SessionLifecycleState::Idle,
        "lifecycle_state transitions to Idle after Connected",
    );
}

/// M2 regression: a ConnectionFailed for an active spawn bucket
/// must reset its `lifecycle_state` to `Sleeping`. Without this,
/// the failed bucket keeps showing the Spawning glyph in the
/// Projects pane forever.
#[test]
fn lifecycle_state_sleeping_after_connection_failed() {
    let mut app = App::test_default();
    app.sessions.clear();

    let spawn_key = SessionKey::from_str_for_test("__spawn_failing__");
    let mut bucket = forge_tui::app::session::Session::new(spawn_key.clone());
    bucket.lifecycle_state = SessionLifecycleState::Spawning;
    app.sessions.insert(spawn_key.clone(), bucket);
    // Not active — this is the background-spawn failure path.
    let key_other = SessionKey::from_str_for_test("other");
    app.sessions
        .insert(key_other.clone(), forge_tui::app::session::Session::new(key_other.clone()));
    app.active_session_key = Some(key_other);

    handle_client_event(
        &mut app,
        ClientEvent::ConnectionFailed {
            session_key: spawn_key.clone(),
            message: "spawn failed".to_owned(),
        },
    );

    let bucket = app.sessions.get(&spawn_key).expect("failed bucket");
    assert_eq!(
        bucket.lifecycle_state,
        SessionLifecycleState::Sleeping,
        "lifecycle_state lands back in Sleeping after a connection failure",
    );
}
