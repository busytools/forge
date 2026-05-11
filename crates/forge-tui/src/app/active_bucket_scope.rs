//! Helper that pivots `App.active_session_key` to a target bucket
//! around a closure, then restores the App-level UI state. The
//! pre-Phase-4 ad-hoc snapshot-restore pattern (a manual sequence of
//! `let prior_* = ...;` saves followed by manual restore) is
//! encapsulated here in a single safe abstraction so callers don't
//! reimplement it and the boundary is auditable.
//!
//! Used by the background-session SDK message dispatcher and the
//! background-Connected welcome/history-replay path: both need to
//! address a bucket that's not the user's currently-rendered
//! session via the App-level accessors (`active_messages_mut`,
//! `active_viewport_mut`, …), and both must leave the user's
//! visible UI state untouched.

use forge_workspace::SessionKey;

use crate::app::App;

/// Run `body` against `app` with `active_session_key` temporarily
/// pivoted to `target_key`. Restores the user-visible UI state
/// (`active_session_key`, input draft, status) after `body` returns.
pub(crate) fn with_pivoted<F, R>(app: &mut App, target_key: SessionKey, body: F) -> R
where
    F: FnOnce(&mut App) -> R,
{
    let prior_active = app.active_session_key.clone();
    let prior_input = app.input.text();
    let prior_status = app.status.clone();
    app.active_session_key = Some(target_key);
    let r = body(app);
    app.active_session_key = prior_active;
    app.input.clear();
    if !prior_input.is_empty() {
        app.input.set_text(&prior_input);
    }
    app.status = prior_status;
    r
}
