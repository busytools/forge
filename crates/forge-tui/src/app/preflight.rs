//! Preflight view state + keyboard handling.
//!
//! Preflight is the first thing forge renders on every route (the
//! renderer lives in [`crate::ui::preflight`]). It runs once per forge
//! run and, once every account has authenticated and every configured
//! dictation model is loaded, hands over to wherever the invocation was
//! headed: chat when forge was given a project, the project picker when
//! it was not.

use crossterm::event::{KeyCode, KeyEvent};

use super::App;
use super::view::{ActiveView, set_active_view};

/// Everything preflight does per tick of the run loop.
///
/// One entry point rather than two calls, so a test can exercise what
/// the loop exercises. Both of these used to run from the renderer,
/// where every `paint()` in the preflight tests reached them for free;
/// moving them here was right - handing over is a view transition - and
/// it silently took that coverage with it.
pub fn tick(app: &mut App) {
    advance(app);
    quit_after_cancel(app);
}

/// Hand over once preflight has nothing left to wait for: to chat when
/// forge was given a project, to the project picker when it was not.
///
/// Called per tick rather than from the renderer, because handing over
/// is a view transition and the renderer is not where those belong.
///
/// The latch is load-bearing rather than an optimisation. A token
/// expiring mid-session takes an account `Ready -> Bailed -> Loading`,
/// so the readiness condition genuinely goes false again while the user
/// is working - and without the latch that would drop them back onto a
/// boot screen from whatever they were doing. The launchpad's own gate
/// covers that window instead, by making rows unclickable and saying
/// why.
pub fn advance(app: &mut App) {
    if app.preflight_done
        || app.active_view != ActiveView::Launchpad
        || !crate::ui::preflight::is_complete(app)
    {
        return;
    }
    app.preflight_done = true;
    app.needs_redraw = true;
    if app.startup_project.is_some() {
        set_active_view(app, ActiveView::Chat);
    }
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
    /// a boot screen out of whatever they were doing.
    #[test]
    fn handing_over_latches() {
        let mut app = App::test_default();
        // No workspace, so the readiness condition is false.
        app.workspace = None;
        app.active_view = ActiveView::Launchpad;
        tick(&mut app);
        assert!(!app.preflight_done, "an unready preflight does not hand over");

        app.preflight_done = true;
        app.active_view = ActiveView::Chat;
        tick(&mut app);
        assert!(
            app.preflight_done,
            "the handover must survive the readiness condition going false again",
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
