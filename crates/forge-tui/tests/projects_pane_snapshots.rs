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
use forge_tui::ui::projects_pane;
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
        PaneHitTarget::SessionRow { .. } => panic!("first hit-target should be ProjectHeader"),
    }
    match &app.pane_hit_targets[1] {
        PaneHitTarget::SessionRow { session_key, y, height } => {
            assert_eq!(session_key, &lead_key);
            assert_eq!(*y, 4, "drilldown session row sits immediately under its project");
            assert_eq!(*height, 1);
        }
        PaneHitTarget::ProjectHeader { .. } => panic!("second hit-target should be SessionRow"),
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
        PaneHitTarget::SessionRow { .. } => panic!("expected ProjectHeader for alpha"),
    }
    // Second stamp is alpha's session row.
    match &app.pane_hit_targets[1] {
        PaneHitTarget::SessionRow { session_key, .. } => {
            assert_eq!(session_key, &alpha_key);
        }
        PaneHitTarget::ProjectHeader { .. } => panic!("expected SessionRow for alpha drilldown"),
    }
    // Third stamp is bravo's project header (no drilldown for inactive).
    match &app.pane_hit_targets[2] {
        PaneHitTarget::ProjectHeader { project_name, .. } => {
            assert_eq!(project_name, "bravo");
        }
        PaneHitTarget::SessionRow { .. } => panic!("expected ProjectHeader for bravo"),
    }
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
