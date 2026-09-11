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
    // Project name ("stargate-chain-pulse" = 20 chars) overflows the
    // 18-char Medium project budget (20 - 2 indent = 18).
    let projects = vec![project_view("stargate-chain-pulse", vec![long_session.clone()])];

    let lead_key = SessionKey::from_str_for_test("really-long-session-id");
    app.sessions.insert(lead_key.clone(), UiSession::new(lead_key.clone()));
    app.active_session_key = Some(lead_key);

    // Medium tier renders in a 24ch-wide pane (PANE_WIDTH_MEDIUM).
    let lines = render_to_lines(&mut app, &projects, 24, 20);

    // Project header truncated. Org-grouped row chrome is 13 chars
    // (`<2 pad><3 connector><1 glyph><1 sp><name><1 sp><3 button>
    // <2 gutter>`), so at width 24 the name budget is 11. 10-char
    // prefix + ellipsis = "stargate-c…". The longest substring of
    // the original we expect to still see is "stargate".
    let any_truncated_project = lines.iter().any(|l| l.contains('…') && l.contains("stargate"));
    assert!(
        any_truncated_project,
        "expected truncated project label in pane output, got: {lines:?}"
    );

    // Session label truncated: "really-long-feature-branch" is 26
    // chars, Medium budget is 8 (20 - 8 left chrome - 4 right time
    // column), so we expect 7 chars + `…` = "really-…".
    // Hit-target stamps must still carry the un-truncated project key.
    let project_target_full = app.pane_hit_targets.iter().any(|t| match t {
        PaneHitTarget::ProjectHeader { project_name, .. } => project_name == "stargate-chain-pulse",
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
    all_glyph_fgs(buffer, glyph).into_iter().next()
}

/// Foreground colors of every cell whose symbol starts with `glyph`,
/// in paint order. Lets a test compare the same glyph across two rows
/// (selected vs background) instead of only naming the first hit.
fn all_glyph_fgs(buffer: &ratatui::buffer::Buffer, glyph: char) -> Vec<ratatui::style::Color> {
    let area = buffer.area();
    let mut fgs = Vec::new();
    for y in 0..area.height {
        for x in 0..area.width {
            if let Some(cell) = buffer.cell((x, y))
                && cell.symbol().starts_with(glyph)
            {
                fgs.push(cell.fg);
            }
        }
    }
    fgs
}

#[test]
fn spinner_glyph_identical_selected_and_unselected() {
    let mut app = App::test_default();

    // Two projects, both leads Running, the first one selected. The
    // spinner reads only the session's own state - selection styles
    // the row label, never the glyph.
    let projects = vec![
        project_view("forge", vec![session_view("session-r", "lead-a")]),
        project_view("stargate", vec![session_view("session-s", "lead-b")]),
    ];

    let key_a = SessionKey::from_str_for_test("session-r");
    let key_b = SessionKey::from_str_for_test("session-s");
    app.active_session_key = Some(key_a.clone());
    register_lifecycle_for_test(&mut app, &key_a, SessionLifecycleState::Running);
    register_lifecycle_for_test(&mut app, &key_b, SessionLifecycleState::Running);

    let backend = TestBackend::new(40, 14);
    let mut terminal = Terminal::new(backend).unwrap();
    let area = Rect::new(0, 0, 40, 14);
    terminal.draw(|frame| projects_pane::render(frame, area, &mut app, &projects)).unwrap();
    let buffer = terminal.backend().buffer().clone();

    let fgs = all_glyph_fgs(&buffer, '\u{280b}');
    assert_eq!(fgs.len(), 2, "both running rows render the spinner, got: {fgs:?}");
    assert_eq!(
        fgs[0], fgs[1],
        "the spinner must render identically on the selected and the background row, got: {fgs:?}",
    );
    assert_eq!(
        fgs[0],
        ratatui::style::Color::Reset,
        "the spinner is terminal-default regardless of selection, got: {:?}",
        fgs[0],
    );
}

#[test]
fn idle_glyph_identical_selected_and_unselected() {
    let mut app = App::test_default();

    // Same property for the settled bullet: an Idle session renders
    // the same `●` selected and unselected; selection styles only the
    // label.
    let projects = vec![
        project_view("forge", vec![session_view("session-i", "lead-a")]),
        project_view("stargate", vec![session_view("session-j", "lead-b")]),
    ];

    let key_a = SessionKey::from_str_for_test("session-i");
    let key_b = SessionKey::from_str_for_test("session-j");
    app.active_session_key = Some(key_a.clone());
    register_lifecycle_for_test(&mut app, &key_a, SessionLifecycleState::Idle);
    register_lifecycle_for_test(&mut app, &key_b, SessionLifecycleState::Idle);

    let backend = TestBackend::new(40, 14);
    let mut terminal = Terminal::new(backend).unwrap();
    let area = Rect::new(0, 0, 40, 14);
    terminal.draw(|frame| projects_pane::render(frame, area, &mut app, &projects)).unwrap();
    let buffer = terminal.backend().buffer().clone();

    let fgs = all_glyph_fgs(&buffer, '\u{25cf}');
    assert_eq!(fgs.len(), 2, "both settled rows render the bullet, got: {fgs:?}");
    assert_eq!(
        fgs[0], fgs[1],
        "the bullet must render identically on the selected and the background row, got: {fgs:?}",
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
    // `stargate`. A PermissionRequest lands on the background session;
    // the projects pane must surface the yellow △ on the background
    // row so the user notices it without switching focus.
    let projects = vec![
        project_view("forge", vec![session_view("session-a", "lead-a")]),
        project_view("stargate", vec![session_view("session-b", "lead-b")]),
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
        project_view("stargate", vec![session_view("session-b", "lead-b")]),
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

fn build_question_request() -> forge_primitives::question::QuestionRequest {
    forge_primitives::question::QuestionRequest {
        tool_call: ToolCall {
            tool_call_id: "tc-test".into(),
            title: "AskUserQuestion".into(),
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
        prompt: forge_primitives::question::QuestionPrompt {
            question: "Pick a colour".into(),
            header: "Colour".into(),
            multi_select: false,
            options: vec![forge_primitives::question::QuestionOption {
                option_id: "q0".into(),
                label: "Red".into(),
                description: None,
                preview: None,
                recommended: false,
            }],
        },
        question_index: 0,
        total_questions: 1,
    }
}

#[test]
fn focused_session_with_pending_prompt_renders_yellow_triangle() {
    let mut app = App::test_default();

    // Single project, focused session mid-AskUserQuestion. The pending
    // prompt is the session's own state, so the row surfaces the yellow
    // △ whether or not it is the one the user is looking at; selection
    // shows in the label highlight instead.
    let projects = vec![project_view("forge", vec![session_view("session-a", "lead-a")])];

    let key_a = SessionKey::from_str_for_test("session-a");
    app.active_session_key = Some(key_a.clone());
    register_lifecycle_for_test(&mut app, &key_a, SessionLifecycleState::Running);

    apply_session_update(
        &mut app,
        SessionUpdate::QuestionRequest {
            key: key_a.clone(),
            tool_id: "tc-test".into(),
            request: build_question_request(),
        },
    );

    let backend = TestBackend::new(26, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let area = Rect::new(0, 0, 26, 10);
    terminal.draw(|frame| projects_pane::render(frame, area, &mut app, &projects)).unwrap();
    let buffer = terminal.backend().buffer().clone();

    let fg = find_glyph_fg(&buffer, '\u{25b3}')
        .expect("focused session with a pending prompt must surface the yellow \u{25b3}");
    assert_eq!(
        fg,
        ratatui::style::Color::Yellow,
        "pending-prompt \u{25b3} must use STATUS_WARNING (Yellow), got: {fg:?}",
    );
}

/// The worker is selected while its lead sits mid-AskUserQuestion.
/// Exactly one row highlights - the selected worker's - and the lead
/// row keeps its own state glyph (yellow △ for the pending question)
/// with no highlight. The worker is seeded in the fresh-spawned shape
/// (its bucket's cwd_raw is the project root and the catalog lists
/// it), so every signal that once dragged the highlight onto the lead
/// row would fail this test.
#[test]
fn worker_selection_highlights_only_the_worker_row() {
    let mut app = App::test_default();
    let workspace = app.workspace.clone().expect("workspace stub");

    let projects = vec![project_view(
        "forge",
        vec![session_view("lead-a", "lead-a"), session_view("worker-1", "reviewer")],
    )];

    let lead_key = SessionKey::from_str_for_test("lead-a");
    let worker_key = SessionKey::from_str_for_test("worker-1");

    // Lead mid-AskUserQuestion: turn in flight, question pending.
    register_lifecycle_for_test(&mut app, &lead_key, SessionLifecycleState::Running);
    app.sessions.get_mut(&lead_key).expect("lead bucket").cwd_raw = "~/Projects/forge".to_owned();
    apply_session_update(
        &mut app,
        SessionUpdate::QuestionRequest {
            key: lead_key.clone(),
            tool_id: "tc-test".into(),
            request: build_question_request(),
        },
    );

    // Fresh-spawned worker shape: bucket cwd_raw on the project root.
    let worker_bucket = app
        .sessions
        .entry(worker_key.clone())
        .or_insert_with(|| UiSession::new(worker_key.clone()));
    worker_bucket.cwd_raw = "~/Projects/forge".to_owned();
    app.active_session_key = Some(worker_key.clone());

    workspace.insert_live_worker(
        &ProjectKey::new_for_test("forge"),
        forge_workspace::WorkerEntry {
            label: "reviewer".into(),
            charter: "be sharp".into(),
            session_key: worker_key.clone(),
            status: forge_primitives::WorkerLiveness::Running,
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by_session_id: "lead-a".into(),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        },
    );

    let backend = TestBackend::new(40, 14);
    let mut terminal = Terminal::new(backend).unwrap();
    let area = Rect::new(0, 0, 40, 14);
    terminal.draw(|frame| projects_pane::render(frame, area, &mut app, &projects)).unwrap();
    let buffer = terminal.backend().buffer().clone();

    // (a) The lead row shows the yellow △ for its own pending question...
    let fg = find_glyph_fg(&buffer, '\u{25b3}').expect("lead row surfaces the yellow \u{25b3}");
    assert_eq!(fg, ratatui::style::Color::Yellow, "lead \u{25b3} must be STATUS_WARNING");
    // ...and is not highlighted.
    let rust_orange = ratatui::style::Color::Rgb(244, 118, 0);
    let row_texts: Vec<(u16, String)> = (0..buffer.area().height)
        .map(|y| {
            let text: String = (0..buffer.area().width)
                .map(|x| {
                    buffer.cell((x, y)).map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                })
                .collect();
            (y, text)
        })
        .collect();
    let lead_row = row_texts
        .iter()
        .find(|(_, t)| t.contains("forge"))
        .map(|(y, _)| *y)
        .expect("lead row renders");
    let lead_row_orange = (0..buffer.area().width)
        .any(|x| buffer.cell((x, lead_row)).is_some_and(|c| c.fg == rust_orange));
    assert!(!lead_row_orange, "the lead row must not highlight while its worker is selected");

    // (b) ...and only the worker row carries the highlight.
    let worker_row = row_texts
        .iter()
        .find(|(_, text)| text.contains("reviewer"))
        .map(|(y, _)| *y)
        .expect("worker row renders");
    for (y, _) in &row_texts {
        // Row 0 is the `PROJECTS` banner - rust orange chrome, not a
        // row highlight.
        if *y == 0 {
            continue;
        }
        let row_has_orange = (0..buffer.area().width)
            .any(|x| buffer.cell((x, *y)).is_some_and(|c| c.fg == rust_orange));
        if row_has_orange {
            assert_eq!(
                *y, worker_row,
                "only the selected worker row may carry the highlight, row {y} also does",
            );
        }
    }
    let worker_label_orange_bold = (0..buffer.area().width).any(|x| {
        buffer.cell((x, worker_row)).is_some_and(|c| {
            c.fg == rust_orange && c.modifier.contains(ratatui::style::Modifier::BOLD)
        })
    });
    assert!(
        worker_label_orange_bold,
        "the selected worker row must be highlighted (rust orange bold label)",
    );
}
