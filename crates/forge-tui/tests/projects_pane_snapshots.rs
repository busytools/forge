//! Snapshot-style tests for the Wide-tier Projects pane. Construct
//! synthetic [`ProjectView`] fixtures via the `test-helpers` feature
//! on `forge-workspace`, render to a [`TestBackend`], and assert the
//! rendered text + hit-target stamps.
//!
//! No real `Workspace` needed: the renderer takes a `&[ProjectView]`
//! slice so tests can build view fixtures directly without spinning
//! up a tempdir + on-disk session catalogs.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_tui::app::App;
use forge_tui::app::PaneHitTarget;
use forge_tui::app::session::{Session, SessionLifecycleState};
use forge_tui::ui::{projects_pane, top_bar};
use forge_workspace::{ProjectKey, ProjectView, SessionKey, SessionView};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

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
fn renders_banner_and_active_project_with_drilldown() {
    let mut app = App::test_default();
    let session_a = session_view("session-a", "main");
    let projects = vec![project_view("forge", vec![session_a.clone()])];

    // Insert a Session bucket for the lead and mark it active so the
    // pane treats `forge` as the active project.
    let lead_key = SessionKey::from_str_for_test("session-a");
    let mut lead_session = Session::new(lead_key.clone());
    lead_session.lifecycle_state = SessionLifecycleState::Idle;
    app.sessions.insert(lead_key.clone(), lead_session);
    app.active_session_key = Some(lead_key.clone());

    let lines = render_to_lines(&mut app, &projects, 26, 10);

    // Row 0: PROJECTS banner.
    assert!(
        lines[0].starts_with("PROJECTS"),
        "banner row should lead with PROJECTS, got: {:?}",
        lines[0]
    );
    // Row 1: dim rule. We can't assert color through TestBackend's
    // symbol-only buffer, so just verify the rule character is there.
    assert!(
        lines[1].contains('─'),
        "rule row should contain box-drawing char, got: {:?}",
        lines[1]
    );
    // Row 2: blank.
    assert!(lines[2].is_empty(), "row 2 should be blank, got: {:?}", lines[2]);
    // Row 3: project name "forge".
    assert!(
        lines[3].contains("forge"),
        "project header should show project name, got: {:?}",
        lines[3]
    );
    // Row 4: drilldown — lead marker (◆), active marker (•), label "main".
    assert!(lines[4].contains('◆'), "lead marker should appear in drilldown, got: {:?}", lines[4]);
    assert!(
        lines[4].contains('•'),
        "current-session marker should appear in drilldown, got: {:?}",
        lines[4]
    );
    assert!(
        lines[4].contains("main"),
        "session label should appear in drilldown, got: {:?}",
        lines[4]
    );

    // Hit-targets: one project header + one session row.
    assert_eq!(
        app.pane_hit_targets.len(),
        2,
        "should stamp 2 hit-targets, got: {:?}",
        app.pane_hit_targets
    );
    match &app.pane_hit_targets[0] {
        PaneHitTarget::ProjectHeader { project_name, y, height } => {
            assert_eq!(project_name, "forge");
            assert_eq!(*y, 3, "project header sits on row 3 (after banner + rule + blank)");
            assert_eq!(*height, 1);
        }
        other => panic!("first hit-target should be ProjectHeader, got {other:?}"),
    }
    match &app.pane_hit_targets[1] {
        PaneHitTarget::SessionRow { session_key, y, height } => {
            assert_eq!(session_key, &lead_key);
            assert_eq!(*y, 4, "drilldown session row sits immediately under its project");
            assert_eq!(*height, 1);
        }
        other => panic!("second hit-target should be SessionRow, got {other:?}"),
    }
}

#[test]
fn inactive_project_drilldown_collapsed_and_one_hit_target_per_row() {
    let mut app = App::test_default();

    // Two projects: alpha + bravo. alpha is active.
    let alpha_session = session_view("alpha-1", "lead");
    let bravo_session = session_view("bravo-1", "main");
    let projects = vec![
        project_view("alpha", vec![alpha_session.clone()]),
        project_view("bravo", vec![bravo_session.clone()]),
    ];

    let alpha_key = SessionKey::from_str_for_test("alpha-1");
    app.sessions.insert(alpha_key.clone(), Session::new(alpha_key.clone()));
    app.active_session_key = Some(alpha_key.clone());

    let lines = render_to_lines(&mut app, &projects, 26, 12);

    // alpha is the active project so it gets a drilldown row; bravo
    // is inactive and collapses to its header. Sort order with no
    // last_activity is alphabetical, so alpha comes first.
    let alpha_row = lines.iter().position(|l| l.contains("alpha")).expect("alpha row");
    let bravo_row = lines.iter().position(|l| l.contains("bravo")).expect("bravo row");
    assert!(alpha_row < bravo_row, "alpha should sort before bravo");

    // alpha has a drilldown row immediately after it (◆ marker).
    let alpha_drilldown = &lines[alpha_row + 1];
    assert!(
        alpha_drilldown.contains('◆'),
        "alpha's drilldown row should show lead marker, got: {alpha_drilldown:?}"
    );

    // bravo's row (the row after alpha's drilldown) should NOT contain ◆ —
    // bravo is inactive, so no drilldown.
    let bravo_drilldown_check = &lines[bravo_row];
    assert!(bravo_drilldown_check.contains("bravo"));
    assert!(
        !bravo_drilldown_check.contains('◆'),
        "inactive project shows no drilldown, got: {bravo_drilldown_check:?}"
    );

    // Hit-targets: alpha header + alpha drilldown session + bravo header = 3.
    assert_eq!(
        app.pane_hit_targets.len(),
        3,
        "stamps: alpha header + alpha session + bravo header, got: {:?}",
        app.pane_hit_targets
    );

    // First stamp is alpha's project header.
    match &app.pane_hit_targets[0] {
        PaneHitTarget::ProjectHeader { project_name, .. } => {
            assert_eq!(project_name, "alpha");
        }
        other => panic!("expected ProjectHeader for alpha, got {other:?}"),
    }
    // Second stamp is alpha's session row.
    match &app.pane_hit_targets[1] {
        PaneHitTarget::SessionRow { session_key, .. } => {
            assert_eq!(session_key, &alpha_key);
        }
        other => panic!("expected SessionRow for alpha drilldown, got {other:?}"),
    }
    // Third stamp is bravo's project header (no drilldown for inactive).
    match &app.pane_hit_targets[2] {
        PaneHitTarget::ProjectHeader { project_name, .. } => {
            assert_eq!(project_name, "bravo");
        }
        other => panic!("expected ProjectHeader for bravo, got {other:?}"),
    }
}

#[test]
fn medium_tier_truncates_long_project_and_session_labels() {
    let mut app = App::test_default();

    // Lead session — long label so the drilldown row will overflow
    // the 12-char Medium session budget (20 - 8 chrome = 12).
    let long_session = session_view("really-long-session-id", "really-long-feature-branch");
    // Project name ("subspace-chain-pulse" = 20 chars) overflows the
    // 18-char Medium project budget (20 - 2 indent = 18).
    let projects = vec![project_view("subspace-chain-pulse", vec![long_session.clone()])];

    let lead_key = SessionKey::from_str_for_test("really-long-session-id");
    app.sessions.insert(lead_key.clone(), Session::new(lead_key.clone()));
    app.active_session_key = Some(lead_key);

    // Medium tier renders in a 20ch-wide pane.
    let lines = render_to_lines(&mut app, &projects, 20, 20);

    // Project header truncated: name had 20 chars, budget is 18, so
    // we expect 17 chars of the name + `…`. The 17-char prefix of
    // "subspace-chain-pulse" is "subspace-chain-pu".
    let any_truncated_project =
        lines.iter().any(|l| l.contains('…') && l.contains("subspace-chain-pu"));
    assert!(
        any_truncated_project,
        "expected truncated project label in pane output, got: {lines:?}"
    );

    // Session label truncated: "really-long-feature-branch" is 26
    // chars, budget is 12, so we expect 11 chars + `…` = "really-long…".
    let any_truncated_session = lines.iter().any(|l| l.contains('…') && l.contains("really-long"));
    assert!(
        any_truncated_session,
        "expected truncated session label in pane output, got: {lines:?}"
    );

    // Hit-target stamps must still carry the un-truncated project
    // name + session key so click routing works.
    let project_target_full = app.pane_hit_targets.iter().any(|t| match t {
        PaneHitTarget::ProjectHeader { project_name, .. } => project_name == "subspace-chain-pulse",
        _ => false,
    });
    assert!(
        project_target_full,
        "project hit-target should retain full un-truncated name, got: {:?}",
        app.pane_hit_targets
    );
    let session_target_full = app.pane_hit_targets.iter().any(|t| match t {
        PaneHitTarget::SessionRow { session_key, .. } => {
            session_key == &SessionKey::from_str_for_test("really-long-session-id")
        }
        _ => false,
    });
    assert!(
        session_target_full,
        "session hit-target should retain full un-truncated key, got: {:?}",
        app.pane_hit_targets
    );
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
    app.sessions.insert(key_a.clone(), Session::new(key_a.clone()));
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
    app.sessions.insert(lead_key.clone(), Session::new(lead_key.clone()));
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
fn narrow_overlay_keeps_full_unmodified_session_key_in_targets() {
    // Same invariant the inline pane upholds: hit-target stamps
    // carry the full identifier even if the rendered label was
    // truncated.
    let mut app = App::test_default();
    let projects = vec![project_view(
        "really-long-project-name",
        vec![session_view("really-long-session-id", "lead")],
    )];
    let lead_key = SessionKey::from_str_for_test("really-long-session-id");
    app.sessions.insert(lead_key.clone(), Session::new(lead_key.clone()));
    app.active_session_key = Some(lead_key.clone());

    // Render at narrow width so labels likely overflow.
    let _lines = render_overlay_to_lines(&mut app, &projects, 60, 20);

    let session_target_full = app.pane_hit_targets.iter().any(|t| match t {
        PaneHitTarget::SessionRow { session_key, .. } => session_key == &lead_key,
        _ => false,
    });
    assert!(
        session_target_full,
        "session hit-target must retain full key, got: {:?}",
        app.pane_hit_targets
    );

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

#[test]
fn sleeping_lead_shows_dot_glyph() {
    let mut app = App::test_default();

    // Project's lead is in `app.sessions` but stays in the default
    // Sleeping lifecycle state. The drilldown row should pick up
    // the sleeping `·` glyph.
    let projects = vec![project_view("forge", vec![session_view("session-z", "lead")])];

    let lead_key = SessionKey::from_str_for_test("session-z");
    let lead_session = Session::new(lead_key.clone()); // default is Sleeping
    app.sessions.insert(lead_key.clone(), lead_session);
    app.active_session_key = Some(lead_key);

    let lines = render_to_lines(&mut app, &projects, 26, 10);

    // Find the drilldown row (the one with ◆).
    let drilldown = lines.iter().find(|l| l.contains('◆')).expect("drilldown row");
    assert!(
        drilldown.contains('·'),
        "sleeping lifecycle should render with · glyph, got: {drilldown:?}"
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
    let mut lead_session = Session::new(lead_key.clone());
    lead_session.lifecycle_state = SessionLifecycleState::Running;
    app.sessions.insert(lead_key.clone(), lead_session);
    app.active_session_key = Some(lead_key);

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
    let mut lead_session = Session::new(lead_key.clone());
    lead_session.lifecycle_state = SessionLifecycleState::Attention;
    app.sessions.insert(lead_key.clone(), lead_session);
    app.active_session_key = Some(lead_key);

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

#[test]
fn wide_tier_lead_plus_extras_renders_diamond_only_on_lead() {
    let mut app = App::test_default();

    // Three sessions on the active project; the ◆ marker must only
    // appear on row 0 (the lead). Subsequent rows just blank-pad
    // the lead column.
    let session_lead = session_view("s-lead", "main");
    let session_b = session_view("s-b", "feat");
    let session_c = session_view("s-c", "fix");
    let projects = vec![project_view(
        "forge",
        vec![session_lead.clone(), session_b.clone(), session_c.clone()],
    )];

    let lead_key = SessionKey::from_str_for_test("s-lead");
    app.sessions.insert(lead_key.clone(), Session::new(lead_key.clone()));
    let key_b = SessionKey::from_str_for_test("s-b");
    app.sessions.insert(key_b.clone(), Session::new(key_b));
    let key_c = SessionKey::from_str_for_test("s-c");
    app.sessions.insert(key_c.clone(), Session::new(key_c));
    app.active_session_key = Some(lead_key);

    let lines = render_to_lines(&mut app, &projects, 26, 12);

    // Header row + 3 drilldown rows; only the first drilldown carries
    // the ◆ marker. Find every row that mentions a session label and
    // count diamonds.
    let drilldown_rows: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("main") || l.contains("feat") || l.contains("fix"))
        .collect();
    assert_eq!(drilldown_rows.len(), 3, "expected 3 drilldown rows, got: {drilldown_rows:?}");

    let main_row = drilldown_rows
        .iter()
        .find(|l| l.contains("main"))
        .expect("lead row containing 'main' label");
    assert!(main_row.contains('◆'), "lead drilldown row should carry ◆, got: {main_row:?}");

    let feat_row = drilldown_rows
        .iter()
        .find(|l| l.contains("feat"))
        .expect("non-lead row containing 'feat' label");
    assert!(!feat_row.contains('◆'), "non-lead drilldown row must not carry ◆, got: {feat_row:?}");

    let fix_row = drilldown_rows
        .iter()
        .find(|l| l.contains("fix"))
        .expect("non-lead row containing 'fix' label");
    assert!(!fix_row.contains('◆'), "non-lead drilldown row must not carry ◆, got: {fix_row:?}");
}
