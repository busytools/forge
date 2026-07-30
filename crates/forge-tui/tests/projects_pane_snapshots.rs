//! Snapshot-style tests for the Wide-tier Projects pane. Construct
//! synthetic [`ProjectView`] fixtures via the `test-helpers` feature
//! on `forge-workspace`, render to a [`TestBackend`], and assert the
//! rendered text + hit-target stamps.
//!
//! No real `Workspace` needed: the renderer takes a `&[ProjectView]`
//! slice so tests can build view fixtures directly without spinning
//! up a tempdir + on-disk session catalogs.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_primitives::permission_ui::{
    PermissionAction, PermissionOption, PermissionOptionKind, PermissionRequest,
};
use forge_primitives::session_update::ToolCall;
use forge_tui::app::App;
use forge_tui::app::PaneHitTarget;
use forge_tui::app::apply_session_update;
use forge_tui::app::session::{SessionLifecycleState, UiSession};
use forge_tui::ui::{projects_pane, top_bar};
use forge_workspace::{ProjectKey, ProjectView, SessionKey, SessionUpdate, SessionView};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

/// Insert (or update) a `UiSession` bucket for `key` carrying
/// `lifecycle_state`. The Projects pane reads lifecycle directly off
/// the bucket; no workspace lookup needed.
fn register_lifecycle_for_test(app: &mut App, key: &SessionKey, state: SessionLifecycleState) {
    let bucket = app.sessions.entry(key.clone()).or_insert_with(|| UiSession::new(key.clone()));
    bucket.lifecycle_state = state;
}

fn render_to_lines(
    app: &mut App,
    projects: &[ProjectView],
    width: u16,
    height: u16,
) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let area = Rect::new(0, 0, width, height);
    terminal.draw(|frame| projects_pane::render(frame, area, app, projects)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| {
                    buffer.cell((x, y)).map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                })
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

fn project_view(name: &str, sessions: Vec<SessionView>) -> ProjectView {
    ProjectView::new_for_test(
        ProjectKey::new_for_test(name),
        name,
        format!("~/Projects/{name}"),
        sessions,
    )
}

fn session_view(id: &str, label: &str) -> SessionView {
    SessionView::new_for_test(SessionKey::from_str_for_test(id), label, false, None)
}

#[test]
fn renders_banner_and_project_row_under_org_header() {
    let mut app = App::test_default();
    let session_a = session_view("session-a", "main");
    let projects = vec![project_view("forge", vec![session_a.clone()])];

    // Insert a Session bucket for the lead so the pane treats `forge`
    // as a live project (gets the close-affordance + active glyph).
    let lead_key = SessionKey::from_str_for_test("session-a");
    let lead_session = UiSession::new(lead_key.clone());
    app.sessions.insert(lead_key.clone(), lead_session);
    app.active_session_key = Some(lead_key.clone());
    register_lifecycle_for_test(&mut app, &lead_key, SessionLifecycleState::Idle);

    let lines = render_to_lines(&mut app, &projects, 26, 10);

    // Row layout: banner, rule, org header ("Test" - set by
    // `ProjectView::new_for_test`), `│` continuation, project row
    // under tree connector `└─`. The continuation visually links
    // the header down to the first project rather than floating
    // disconnected above an empty gap.
    assert!(lines[0].contains("PROJECTS"), "banner: {:?}", lines[0]);
    assert!(lines[1].contains('─'), "rule: {:?}", lines[1]);
    assert!(lines[2].contains("Test"), "org header: {:?}", lines[2]);
    assert!(lines[3].contains('\u{2502}'), "continuation after header: {:?}", lines[3]);
    let project_row = lines.iter().find(|l| l.contains("forge")).expect("project row");
    assert!(project_row.contains('\u{2514}'), "tree connector \u{2514}: {project_row:?}");

    // Two hit targets for a live row: the project header (full-row
    // click → focus/switch) and the close glyph (right-edge band →
    // kill session).
    let project_header = app.pane_hit_targets.iter().find_map(|t| match t {
        PaneHitTarget::ProjectHeader { project_name, .. } => Some(project_name.clone()),
        _ => None,
    });
    assert_eq!(project_header.as_deref(), Some("forge"), "project header hit target");
    let close_present =
        app.pane_hit_targets.iter().any(|t| matches!(t, PaneHitTarget::CloseSession { .. }));
    assert!(close_present, "live row stamps a CloseSession hit target");
}

#[test]
fn live_and_idle_projects_render_under_same_org_with_distinct_glyphs() {
    // Both projects share the "Test" org (set by
    // `ProjectView::new_for_test`). alpha has a live bucket → gets
    // the spinner/Idle glyph + close-affordance hit target. bravo
    // doesn't → gets the `○` idle glyph + no close target.
    let mut app = App::test_default();

    let alpha_session = session_view("alpha-1", "lead");
    let bravo_session = session_view("bravo-1", "main");
    let projects = vec![
        project_view("alpha", vec![alpha_session.clone()]),
        project_view("bravo", vec![bravo_session.clone()]),
    ];

    let alpha_key = SessionKey::from_str_for_test("alpha-1");
    app.sessions.insert(alpha_key.clone(), UiSession::new(alpha_key.clone()));
    app.active_session_key = Some(alpha_key.clone());

    let lines = render_to_lines(&mut app, &projects, 26, 14);

    let org_header = lines.iter().position(|l| l.contains("Test")).expect("org header");
    let alpha_row = lines.iter().position(|l| l.contains("alpha")).expect("alpha row");
    let bravo_row = lines.iter().position(|l| l.contains("bravo")).expect("bravo row");
    assert!(org_header < alpha_row && org_header < bravo_row, "rows under org header");

    // Two ProjectHeader stamps + exactly one CloseSession (alpha's).
    let header_count = app
        .pane_hit_targets
        .iter()
        .filter(|t| matches!(t, PaneHitTarget::ProjectHeader { .. }))
        .count();
    assert_eq!(header_count, 2, "two project headers, got: {:?}", app.pane_hit_targets);
    let close_count = app
        .pane_hit_targets
        .iter()
        .filter(|t| matches!(t, PaneHitTarget::CloseSession { .. }))
        .count();
    assert_eq!(close_count, 1, "only the live row stamps a CloseSession");
}

#[test]
fn medium_tier_truncates_long_project_labels() {
    let mut app = App::test_default();

    let long_session = session_view("really-long-session-id", "really-long-feature-branch");
    // Project name ("subspace-chain-pulse" = 20 chars) overflows the
    // 18-char Medium project budget (20 - 2 indent = 18).
    let projects = vec![project_view("subspace-chain-pulse", vec![long_session.clone()])];

    let lead_key = SessionKey::from_str_for_test("really-long-session-id");
    app.sessions.insert(lead_key.clone(), UiSession::new(lead_key.clone()));
    app.active_session_key = Some(lead_key);

    // Medium tier renders in a 24ch-wide pane (PANE_WIDTH_MEDIUM).
    let lines = render_to_lines(&mut app, &projects, 24, 20);

    // Project header truncated. Org-grouped row chrome is 13 chars
    // (`<2 pad><3 connector><1 glyph><1 sp><name><1 sp><3 button>
    // <2 gutter>`), so at width 24 the name budget is 11. 10-char
    // prefix + ellipsis = "subspace-c…". The longest substring of
    // the original we expect to still see is "subspace".
    let any_truncated_project = lines.iter().any(|l| l.contains('…') && l.contains("subspace"));
    assert!(
        any_truncated_project,
        "expected truncated project label in pane output, got: {lines:?}"
    );

    // Session label truncated: "really-long-feature-branch" is 26
    // chars, Medium budget is 8 (20 - 8 left chrome - 4 right time
    // column), so we expect 7 chars + `…` = "really-…".
    // Hit-target stamps must still carry the un-truncated project key.
    let project_target_full = app.pane_hit_targets.iter().any(|t| match t {
        PaneHitTarget::ProjectHeader { project_name, .. } => project_name == "subspace-chain-pulse",
        _ => false,
    });
    assert!(
        project_target_full,
        "project hit-target should retain full un-truncated key, got: {:?}",
        app.pane_hit_targets
    );
    let _ = long_session;
}

fn render_overlay_to_lines(
    app: &mut App,
    projects: &[ProjectView],
    width: u16,
    height: u16,
) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let area = Rect::new(0, 0, width, height);
    terminal.draw(|frame| projects_pane::render_overlay(frame, area, app, projects)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| {
                    buffer.cell((x, y)).map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                })
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

fn render_top_bar_to_lines(app: &mut App, width: u16) -> Vec<String> {
    let backend = TestBackend::new(width, 1);
    let mut terminal = Terminal::new(backend).unwrap();
    let area = Rect::new(0, 0, width, 1);
    terminal.draw(|frame| top_bar::render(frame, area, app)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..1)
        .map(|y| {
            (0..width)
                .map(|x| {
                    buffer.cell((x, y)).map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                })
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

#[test]
fn narrow_top_bar_renders_icon_and_stamps_target() {
    let mut app = App::test_default();
    let key_a = SessionKey::from_str_for_test("session-a");
    app.sessions.insert(key_a.clone(), UiSession::new(key_a.clone()));
    app.active_session_key = Some(key_a);

    let lines = render_top_bar_to_lines(&mut app, 100);
    assert_eq!(lines.len(), 1, "top bar is single-row");
    assert!(lines[0].contains('▤'), "top bar shows pane icon, got: {:?}", lines[0]);

    // The icon is at column 0; one TopBarIcon stamp must exist at
    // x_start=0, x_end=1.
    let icon_target = app.pane_hit_targets.iter().find_map(|t| match t {
        PaneHitTarget::TopBarIcon { x_start, x_end, y, height } => {
            Some((*x_start, *x_end, *y, *height))
        }
        _ => None,
    });
    let (x_start, x_end, y, height) = icon_target.expect("top-bar icon target stamped");
    assert_eq!((x_start, x_end), (0, 1), "icon spans column 0 only");
    assert_eq!((y, height), (0, 1), "icon sits on row 0, 1-row tall");
}

#[test]
fn narrow_overlay_banner_includes_close_glyph_and_target() {
    let mut app = App::test_default();
    let projects = vec![project_view("forge", vec![session_view("session-a", "main")])];
    let lead_key = SessionKey::from_str_for_test("session-a");
    app.sessions.insert(lead_key.clone(), UiSession::new(lead_key.clone()));
    app.active_session_key = Some(lead_key);

    let lines = render_overlay_to_lines(&mut app, &projects, 100, 12);
    // Row 0: banner with `▤ PROJECTS` on the left and `✕` on the right.
    assert!(
        lines[0].contains("▤ PROJECTS") && lines[0].contains('✕'),
        "overlay banner should show both labels, got: {:?}",
        lines[0]
    );
    // Row 1: rule. Row 2: blank. Row 3+: project list.
    assert!(lines[1].contains('─'), "rule under banner, got: {:?}", lines[1]);
    assert!(lines[2].is_empty(), "blank row before project list, got: {:?}", lines[2]);
    assert!(
        lines.iter().any(|l| l.contains("forge")),
        "overlay should show project name 'forge', got: {lines:?}"
    );

    // Hit-targets: the OverlayClose stamp should be at the right
    // edge of the banner row.
    let close_target = app.pane_hit_targets.iter().find_map(|t| match t {
        PaneHitTarget::OverlayClose { x_start, x_end, y, height } => {
            Some((*x_start, *x_end, *y, *height))
        }
        _ => None,
    });
    let (x_start, x_end, y, height) = close_target.expect("overlay close target stamped");
    assert_eq!(x_end, 100, "✕ glyph sits at the rightmost column");
    assert!(x_start < x_end);
    assert_eq!((y, height), (0, 1));
}

#[test]
fn narrow_overlay_keeps_full_unmodified_project_key_in_targets() {
    // Hit-target stamps carry the full identifier even if the
    // rendered label was head-truncated.
    let mut app = App::test_default();
    let projects = vec![project_view(
        "really-long-project-name",
        vec![session_view("really-long-session-id", "lead")],
    )];
    let lead_key = SessionKey::from_str_for_test("really-long-session-id");
    app.sessions.insert(lead_key.clone(), UiSession::new(lead_key.clone()));
    app.active_session_key = Some(lead_key);

    let _lines = render_overlay_to_lines(&mut app, &projects, 60, 20);

    let project_target_full = app.pane_hit_targets.iter().any(|t| match t {
        PaneHitTarget::ProjectHeader { project_name, .. } => {
            project_name == "really-long-project-name"
        }
        _ => false,
    });
    assert!(
        project_target_full,
        "project hit-target must retain full name, got: {:?}",
        app.pane_hit_targets
    );
}

/// Find the foreground color of the first cell in `buffer` whose
/// symbol matches `glyph`. Returns `None` if the glyph isn't found.
/// Used by per-state-color tests to look up the glyph's `Color`
/// directly from the rendered buffer rather than just asserting the
/// symbol is present.
fn find_glyph_fg(buffer: &ratatui::buffer::Buffer, glyph: char) -> Option<ratatui::style::Color> {
    let area = buffer.area();
    for y in 0..area.height {
        for x in 0..area.width {
            if let Some(cell) = buffer.cell((x, y))
                && cell.symbol().starts_with(glyph)
            {
                return Some(cell.fg);
            }
        }
    }
    None
}

#[test]
fn wide_tier_running_session_glyph_uses_accent_color() {
    let mut app = App::test_default();

    // Single project, lead session marked Running and active. The
    // spinner glyph (⠋) for an active+Running session must render in
    // RUST_ORANGE per the Projects-pane spec.
    let projects = vec![project_view("forge", vec![session_view("session-r", "lead")])];

    let lead_key = SessionKey::from_str_for_test("session-r");
    let lead_session = UiSession::new(lead_key.clone());
    app.sessions.insert(lead_key.clone(), lead_session);
    app.active_session_key = Some(lead_key.clone());
    register_lifecycle_for_test(&mut app, &lead_key, SessionLifecycleState::Running);

    let backend = TestBackend::new(26, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let area = Rect::new(0, 0, 26, 10);
    terminal.draw(|frame| projects_pane::render(frame, area, &mut app, &projects)).unwrap();
    let buffer = terminal.backend().buffer().clone();

    let fg = find_glyph_fg(&buffer, '⠋').expect("spinner glyph rendered");
    assert_eq!(
        fg,
        ratatui::style::Color::Rgb(244, 118, 0),
        "active+Running spinner must use RUST_ORANGE, got: {fg:?}"
    );
}

#[test]
fn wide_tier_attention_session_glyph_uses_warning_color() {
    let mut app = App::test_default();

    // Lead session marked Attention (a paused background session
    // awaiting permission input). The △ glyph must render in
    // STATUS_WARNING per spec.
    let projects = vec![project_view("forge", vec![session_view("session-a", "lead")])];

    let lead_key = SessionKey::from_str_for_test("session-a");
    let lead_session = UiSession::new(lead_key.clone());
    app.sessions.insert(lead_key.clone(), lead_session);
    app.active_session_key = Some(lead_key.clone());
    register_lifecycle_for_test(&mut app, &lead_key, SessionLifecycleState::Attention);

    let backend = TestBackend::new(26, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let area = Rect::new(0, 0, 26, 10);
    terminal.draw(|frame| projects_pane::render(frame, area, &mut app, &projects)).unwrap();
    let buffer = terminal.backend().buffer().clone();

    let fg = find_glyph_fg(&buffer, '△').expect("attention glyph rendered");
    assert_eq!(
        fg,
        ratatui::style::Color::Yellow,
        "Attention glyph must use STATUS_WARNING (Yellow), got: {fg:?}"
    );
}

fn build_permission_request() -> PermissionRequest {
    PermissionRequest {
        tool_call: ToolCall {
            tool_call_id: "tc-test".into(),
            title: "Bash".into(),
            kind: forge_primitives::ToolKind::Execute,
            status: forge_primitives::ToolCallStatus::Pending,
            content: vec![],
            raw_input: None,
            raw_output: None,
            output_metadata: None,
            task_metadata: None,
            locations: vec![],
            meta: None,
        },
        options: vec![PermissionOption {
            option_id: "allow".into(),
            name: "Allow".into(),
            kind: PermissionOptionKind::Allow,
            action: PermissionAction::Allow,
            recommended: false,
        }],
        display: None,
    }
}

#[test]
fn wide_tier_background_session_with_pending_prompt_renders_yellow_glyph() {
    let mut app = App::test_default();

    // Two projects: active session on `forge`, background session on
    // `subspace`. A PermissionRequest lands on the background session;
    // the projects pane must surface the yellow △ on the background
    // row so the user notices it without switching focus.
    let projects = vec![
        project_view("forge", vec![session_view("session-a", "lead-a")]),
        project_view("subspace", vec![session_view("session-b", "lead-b")]),
    ];

    let key_a = SessionKey::from_str_for_test("session-a");
    let key_b = SessionKey::from_str_for_test("session-b");
    app.active_session_key = Some(key_a.clone());
    register_lifecycle_for_test(&mut app, &key_a, SessionLifecycleState::Idle);
    register_lifecycle_for_test(&mut app, &key_b, SessionLifecycleState::Idle);

    apply_session_update(
        &mut app,
        SessionUpdate::PermissionRequest {
            key: key_b.clone(),
            tool_id: "tc-test".into(),
            request: build_permission_request(),
        },
    );

    let backend = TestBackend::new(40, 14);
    let mut terminal = Terminal::new(backend).unwrap();
    let area = Rect::new(0, 0, 40, 14);
    terminal.draw(|frame| projects_pane::render(frame, area, &mut app, &projects)).unwrap();
    let buffer = terminal.backend().buffer().clone();

    let fg = find_glyph_fg(&buffer, '△')
        .expect("background session with pending prompt must surface △ on its row");
    assert_eq!(
        fg,
        ratatui::style::Color::Yellow,
        "background-row △ must use STATUS_WARNING (Yellow), got: {fg:?}"
    );
}

/// A background session whose turn died surfaces red `✕` on its row -
/// a different glyph and colour from the yellow `△`, because an error
/// is not a request for input.
#[test]
fn wide_tier_background_session_with_failed_turn_renders_red_cross() {
    let mut app = App::test_default();

    let projects = vec![
        project_view("forge", vec![session_view("session-a", "lead-a")]),
        project_view("subspace", vec![session_view("session-b", "lead-b")]),
    ];

    let key_a = SessionKey::from_str_for_test("session-a");
    let key_b = SessionKey::from_str_for_test("session-b");
    app.active_session_key = Some(key_a.clone());
    register_lifecycle_for_test(&mut app, &key_a, SessionLifecycleState::Idle);
    register_lifecycle_for_test(&mut app, &key_b, SessionLifecycleState::Idle);
    app.sessions.get_mut(&key_b).expect("registered bucket").failed_turn =
        Some(forge_tui::app::FailedTurn {
            error: forge_primitives::ApiRetryError::ServerError,
            status: Some(529),
            failed_at: std::time::SystemTime::UNIX_EPOCH,
        });

    let backend = TestBackend::new(40, 14);
    let mut terminal = Terminal::new(backend).unwrap();
    let area = Rect::new(0, 0, 40, 14);
    terminal.draw(|frame| projects_pane::render(frame, area, &mut app, &projects)).unwrap();
    let buffer = terminal.backend().buffer().clone();

    let fg = find_glyph_fg(&buffer, '\u{2715}')
        .expect("background session with a failed turn must surface ✕ on its row");
    assert_eq!(
        fg,
        ratatui::style::Color::Red,
        "background-row ✕ must use STATUS_ERROR (Red), got: {fg:?}"
    );
}

#[test]
fn wide_tier_focused_session_with_pending_prompt_keeps_normal_glyph() {
    let mut app = App::test_default();

    // Single project, focused session has a pending PermissionRequest.
    // The yellow signal is "background session needs you"; the focused
    // row is already in the user's view, so it keeps its normal Idle
    // glyph (no over-trigger).
    let projects = vec![project_view("forge", vec![session_view("session-a", "lead-a")])];

    let key_a = SessionKey::from_str_for_test("session-a");
    app.active_session_key = Some(key_a.clone());
    register_lifecycle_for_test(&mut app, &key_a, SessionLifecycleState::Idle);

    apply_session_update(
        &mut app,
        SessionUpdate::PermissionRequest {
            key: key_a.clone(),
            tool_id: "tc-test".into(),
            request: build_permission_request(),
        },
    );

    let backend = TestBackend::new(26, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let area = Rect::new(0, 0, 26, 10);
    terminal.draw(|frame| projects_pane::render(frame, area, &mut app, &projects)).unwrap();
    let buffer = terminal.backend().buffer().clone();

    assert!(
        find_glyph_fg(&buffer, '△').is_none(),
        "focused session with pending prompt must not flip its row to yellow △",
    );
}
