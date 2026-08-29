//! Preflight view state + keyboard handling.
//!
//! Preflight is the launchpad's first view (the renderer lives in
//! [`crate::ui::preflight`]). It runs once per forge run and hands over
//! to the projects view the moment every account has authenticated and
//! every configured dictation model is loaded.

use crossterm::event::{KeyCode, KeyEvent};

use super::App;

/// `true` once the projects view owns the launchpad. Latches: preflight
/// is shown once per run and never comes back.
///
/// The latch is load-bearing rather than an optimisation. A token
/// expiring mid-session takes an account `Ready -> Bailed -> Loading`,
/// so the readiness condition genuinely goes false again while the user
/// is working - and without the latch that would throw them back onto a
/// boot screen. The launchpad's own gate handles that window by making
/// rows unclickable and saying why.
pub fn hand_over_when_ready(app: &mut App) -> bool {
    if app.preflight_done {
        return true;
    }
    if crate::ui::preflight::is_complete(app) {
        app.preflight_done = true;
        app.needs_redraw = true;
    }
    app.preflight_done
}

/// Handle a key while preflight is up. Always returns `true`: nothing
/// here may leak into a chat input that is not on screen, and the
/// project picker underneath is not reachable yet.
///
/// `Esc` stops an in-flight model download, which quits forge. Every
/// other key is consumed silently; `Ctrl+Q` never reaches here, the
/// always-allowed shortcuts take it first.
pub fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    if key.code == KeyCode::Esc
        && let Some(workspace) = app.workspace.as_ref()
    {
        workspace.cancel_dictate_preflight();
        app.needs_redraw = true;
    }
    true
}

/// Quit once a cancelled preflight has been drawn.
///
/// Cancelling is terminal - there is no dictation-less runtime to fall
/// back to - so forge goes. It goes on the frame AFTER the cancelled
/// state is painted, so the screen gets to say what it kept and where
/// before the terminal is handed back.
pub fn quit_after_cancel(app: &mut App) {
    if app.preflight_done || !app.preflight_cancel_drawn {
        return;
    }
    let cancelled = app
        .workspace
        .as_ref()
        .is_some_and(|ws| ws.dictate_snapshot().failure.is_some_and(|f| f.is_cancelled()));
    if cancelled {
        app.should_quit = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crossterm::event::KeyModifiers;

    /// Once preflight is done it must stay done. Accounts go
    /// `Ready -> Bailed -> Loading` on a mid-session token expiry, so a
    /// condition re-evaluated every frame would drop the user back onto
    /// a boot screen while they were working.
    #[test]
    fn handing_over_latches() {
        let mut app = App::test_default();
        // No workspace, so the readiness condition is false.
        app.workspace = None;
        assert!(!hand_over_when_ready(&mut app), "an unready preflight does not hand over");

        app.preflight_done = true;
        assert!(
            hand_over_when_ready(&mut app),
            "the projects view must survive the readiness condition going false again",
        );
    }

    /// Preflight has to repaint on its own. Account state and dictation
    /// progress are polled rather than pushed, so nothing else in the
    /// run loop marks the screen dirty - without this the spinner is
    /// painted once and stops, and the screen never updates while
    /// accounts resolve or three gigabytes download.
    ///
    /// The change this catches is someone tightening the animation gate
    /// for a repaint-cost reason, which is exactly how it got here.
    #[test]
    fn preflight_animates_until_it_hands_over() {
        let mut app = App::test_default();
        app.active_view = crate::app::ActiveView::Chat;
        app.status = crate::app::AppStatus::Ready;
        assert!(
            !crate::app::is_animating(&app),
            "an idle chat has nothing animating, or this test proves nothing",
        );

        app.active_view = crate::app::ActiveView::Launchpad;
        assert!(
            crate::app::is_animating(&app),
            "preflight must earn a repaint on its own; nothing else marks it dirty",
        );

        app.preflight_done = true;
        assert!(
            !crate::app::is_animating(&app),
            "the projects view is static again once preflight has handed over",
        );
    }

    #[test]
    fn every_key_is_consumed() {
        let mut app = App::test_default();
        for code in [KeyCode::Enter, KeyCode::Char('j'), KeyCode::Up, KeyCode::Esc] {
            assert!(
                handle_key(&mut app, KeyEvent::new(code, KeyModifiers::NONE)),
                "{code:?} must not leak past preflight into a chat input that is not on screen",
            );
        }
    }
}
