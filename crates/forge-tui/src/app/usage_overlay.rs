//! `/usage` overlay state: a token/cost report grouped by project or
//! model over a chosen window, scanned off-thread and priced from the
//! runtime cache.
//!
//! Mirrors the `diff_overlay` shape - a spawn-and-forget scan whose
//! result arrives on a dedicated channel drained each frame - but the
//! scan is `Workspace::scan_usage` (blocking file IO on `spawn_blocking`)
//! rather than an async git subprocess.

use crossterm::event::{KeyCode, KeyEvent};
use forge_primitives::token_usage::{UsageReport, UsageRow, WindowUsage};

use crate::app::App;
use crate::app::view::{ActiveView, set_active_view};

/// Which axis the table groups by; `g` toggles it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grouping {
    Project,
    Model,
}

impl Grouping {
    fn toggled(self) -> Self {
        match self {
            Self::Project => Self::Model,
            Self::Model => Self::Project,
        }
    }
}

/// The rolling window the table shows; `w` cycles it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    Today,
    Week,
    Month,
    Lifetime,
}

impl Window {
    fn next(self) -> Self {
        match self {
            Self::Today => Self::Week,
            Self::Week => Self::Month,
            Self::Month => Self::Lifetime,
            Self::Lifetime => Self::Today,
        }
    }
}

/// Outcome of a background scan delivered to the overlay.
#[derive(Debug)]
pub enum UsageOverlayEvent {
    // Boxed to keep the enum small - a bare `UsageReport` dwarfs the
    // unit `ScanFailed` variant.
    Report(Box<UsageReport>),
    /// The scan task died (a panic surfaces as a dropped `JoinError`);
    /// without this the overlay would hang on "scanning…" forever.
    ScanFailed,
}

/// Overlay presentation state. `report` is `None` until the first scan
/// lands (the overlay shows a scanning notice meanwhile); `scan_failed`
/// flips the notice to a retry hint when a scan died before any report.
#[derive(Debug)]
pub struct UsageOverlayState {
    pub report: Option<UsageReport>,
    pub group: Grouping,
    pub window: Window,
    pub scroll: u16,
    pub scan_failed: bool,
}

impl UsageOverlayState {
    fn new() -> Self {
        // Defaults match the mock: grouped by project, lifetime window.
        Self {
            report: None,
            group: Grouping::Project,
            window: Window::Lifetime,
            scroll: 0,
            scan_failed: false,
        }
    }

    /// The chosen window's data, or `None` before the first scan.
    pub fn window_usage(&self) -> Option<&WindowUsage> {
        self.report.as_ref().map(|report| match self.window {
            Window::Today => &report.today,
            Window::Week => &report.week,
            Window::Month => &report.month,
            Window::Lifetime => &report.lifetime,
        })
    }

    /// The rows for the current grouping, sorted by cost descending.
    pub fn rows(&self) -> &[UsageRow] {
        self.window_usage().map_or(&[], |window| match self.group {
            Grouping::Project => &window.by_project,
            Grouping::Model => &window.by_model,
        })
    }
}

/// Open the overlay and kick the background scan.
pub(crate) fn open(app: &mut App) {
    app.usage_overlay = Some(UsageOverlayState::new());
    set_active_view(app, ActiveView::Usage);
    spawn_fetch(app);
    app.needs_redraw = true;
}

/// Drop the overlay and return to chat.
pub(crate) fn close(app: &mut App) {
    app.usage_overlay = None;
    set_active_view(app, ActiveView::Chat);
    app.needs_redraw = true;
}

/// Scan immediately (tokens now, cost from whatever pricing is cached),
/// then refresh pricing in the background and re-scan only when a new
/// price table was fetched - so the first open with an empty cache fills
/// in costs a moment after the tokens appear.
fn spawn_fetch(app: &mut App) {
    let Some(workspace) = app.workspace.clone() else {
        return;
    };
    let tx = app.usage_overlay_event_tx.clone();
    tokio::task::spawn_local(async move {
        let scan = workspace.clone();
        let _ = tx.send(scan_event(tokio::task::spawn_blocking(move || scan.scan_usage()).await));
        if workspace.refresh_pricing().await {
            let scan = workspace.clone();
            let _ =
                tx.send(scan_event(tokio::task::spawn_blocking(move || scan.scan_usage()).await));
        }
    });
}

/// Map a `spawn_blocking` join result to an overlay event; a `JoinError`
/// (the scan task panicked) becomes the failure sentinel.
fn scan_event(
    joined: Result<forge_primitives::token_usage::UsageReport, tokio::task::JoinError>,
) -> UsageOverlayEvent {
    match joined {
        Ok(report) => UsageOverlayEvent::Report(Box::new(report)),
        Err(error) => {
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                %error,
                "usage scan task failed; showing the retry hint",
            );
            UsageOverlayEvent::ScanFailed
        }
    }
}

/// Apply any scanned reports that arrived since the last frame.
pub(crate) fn drain_events(app: &mut App) {
    while let Ok(event) = app.usage_overlay_event_rx.try_recv() {
        if let Some(overlay) = app.usage_overlay.as_mut() {
            match event {
                UsageOverlayEvent::Report(report) => {
                    overlay.report = Some(*report);
                    overlay.scan_failed = false;
                }
                // Only surfaces in the UI while there's no report to show;
                // a failed re-scan keeps the last good report.
                UsageOverlayEvent::ScanFailed => overlay.scan_failed = true,
            }
            app.needs_redraw = true;
        }
    }
}

pub(crate) fn handle_key(app: &mut App, key: KeyEvent) {
    if key.code == KeyCode::Esc {
        close(app);
        return;
    }
    let Some(overlay) = app.usage_overlay.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Char('g') => {
            overlay.group = overlay.group.toggled();
            overlay.scroll = 0;
        }
        KeyCode::Char('w') => {
            overlay.window = overlay.window.next();
            overlay.scroll = 0;
        }
        KeyCode::Up | KeyCode::Char('k') => overlay.scroll = overlay.scroll.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => overlay.scroll = overlay.scroll.saturating_add(1),
        KeyCode::PageUp => overlay.scroll = overlay.scroll.saturating_sub(10),
        KeyCode::PageDown => overlay.scroll = overlay.scroll.saturating_add(10),
        _ => return,
    }
    app.needs_redraw = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn seed(app: &mut App) {
        app.usage_overlay = Some(UsageOverlayState::new());
    }

    fn overlay(app: &App) -> &UsageOverlayState {
        app.usage_overlay.as_ref().expect("overlay present")
    }

    #[test]
    fn open_transitions_to_usage_view() {
        let mut app = App::test_default();
        // No workspace -> the scan/fetch task is not spawned, so the
        // transition is exercisable without a runtime.
        app.workspace = None;
        open(&mut app);
        assert_eq!(app.active_view, ActiveView::Usage);
        assert!(app.usage_overlay.is_some());
        assert_eq!(overlay(&app).group, Grouping::Project, "defaults to by-project");
        assert_eq!(overlay(&app).window, Window::Lifetime, "defaults to lifetime");
    }

    #[test]
    fn esc_closes_back_to_chat() {
        let mut app = App::test_default();
        app.workspace = None;
        open(&mut app);
        handle_key(&mut app, key(KeyCode::Esc));
        assert_eq!(app.active_view, ActiveView::Chat);
        assert!(app.usage_overlay.is_none());
    }

    #[test]
    fn g_toggles_grouping() {
        let mut app = App::test_default();
        seed(&mut app);
        handle_key(&mut app, key(KeyCode::Char('g')));
        assert_eq!(overlay(&app).group, Grouping::Model);
        handle_key(&mut app, key(KeyCode::Char('g')));
        assert_eq!(overlay(&app).group, Grouping::Project);
    }

    #[test]
    fn w_cycles_windows() {
        let mut app = App::test_default();
        seed(&mut app);
        for expected in [Window::Today, Window::Week, Window::Month, Window::Lifetime] {
            handle_key(&mut app, key(KeyCode::Char('w')));
            assert_eq!(overlay(&app).window, expected);
        }
    }

    #[test]
    fn arrows_adjust_scroll_and_saturate() {
        let mut app = App::test_default();
        seed(&mut app);
        handle_key(&mut app, key(KeyCode::Down));
        handle_key(&mut app, key(KeyCode::Down));
        assert_eq!(overlay(&app).scroll, 2);
        handle_key(&mut app, key(KeyCode::Up));
        assert_eq!(overlay(&app).scroll, 1);
        handle_key(&mut app, key(KeyCode::Up));
        handle_key(&mut app, key(KeyCode::Up));
        assert_eq!(overlay(&app).scroll, 0, "scroll saturates at the top");
    }

    #[test]
    fn changing_group_or_window_resets_scroll() {
        let mut app = App::test_default();
        seed(&mut app);
        handle_key(&mut app, key(KeyCode::Down));
        handle_key(&mut app, key(KeyCode::Char('g')));
        assert_eq!(overlay(&app).scroll, 0, "regrouping returns to the top");
        handle_key(&mut app, key(KeyCode::Down));
        handle_key(&mut app, key(KeyCode::Char('w')));
        assert_eq!(overlay(&app).scroll, 0, "changing window returns to the top");
    }

    fn row(label: &str) -> UsageRow {
        UsageRow {
            label: label.to_owned(),
            input: 0,
            cache_write_1h: 0,
            cache_write_5m: 0,
            cache_read: 0,
            output: 0,
            cost_usd: 0.0,
        }
    }

    fn window(model: &str, project: &str) -> WindowUsage {
        WindowUsage {
            by_model: vec![row(model)],
            by_project: vec![row(project)],
            total: row("TOTAL"),
        }
    }

    #[test]
    fn rows_and_window_track_group_and_window() {
        let report = UsageReport {
            today: window("today-m", "today-p"),
            week: window("week-m", "week-p"),
            month: window("month-m", "month-p"),
            lifetime: window("life-m", "life-p"),
            pricing_available: true,
        };
        let mut state = UsageOverlayState::new();
        state.report = Some(report);

        // Default: lifetime + by project.
        assert_eq!(state.rows()[0].label, "life-p");
        state.group = Grouping::Model;
        assert_eq!(state.rows()[0].label, "life-m");
        state.window = Window::Today;
        assert_eq!(state.rows()[0].label, "today-m");
        assert_eq!(state.window_usage().expect("window").total.label, "TOTAL");
    }

    #[test]
    fn scan_failed_flag_flips_and_a_report_clears_it() {
        let mut app = App::test_default();
        seed(&mut app);
        app.usage_overlay_event_tx.send(UsageOverlayEvent::ScanFailed).expect("send failed");
        drain_events(&mut app);
        assert!(overlay(&app).scan_failed, "a dropped scan flips the failed flag");
        assert!(overlay(&app).report.is_none());

        let report = UsageReport {
            today: window("m", "p"),
            week: window("m", "p"),
            month: window("m", "p"),
            lifetime: window("m", "p"),
            pricing_available: true,
        };
        app.usage_overlay_event_tx
            .send(UsageOverlayEvent::Report(Box::new(report)))
            .expect("send report");
        drain_events(&mut app);
        assert!(!overlay(&app).scan_failed, "a fresh report clears the failed flag");
        assert!(overlay(&app).report.is_some());
    }
}
