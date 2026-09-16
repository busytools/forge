//! `/gateway` overlay: transient state + key handling.
//!
//! A centered, read-only overlay (rendered by
//! [`crate::ui::gateway_view`]) showing what the gateway holds: every
//! org, that org's primary and fallback pins, and each account's live
//! state, snapshotted via
//! [`forge_workspace::Workspace::gateway_view_snapshot`]. Nothing here
//! picks, moves or respawns anything - the spawn pick is the only way a
//! session's account is decided, so there is no account to switch to.
//! `esc` closes.

use crossterm::event::{KeyCode, KeyEvent};
use forge_workspace::GatewayOrgView;

use super::App;

/// State for the open `/gateway` view. `None` on `App` when closed.
#[derive(Debug, Clone)]
pub struct GatewayViewState {
    /// Every org the gateway holds, its pins and its accounts' state.
    pub orgs: Vec<GatewayOrgView>,
}

/// Open the view over `orgs`.
pub(crate) fn open(app: &mut App, orgs: Vec<GatewayOrgView>) {
    app.gateway_view = Some(GatewayViewState { orgs });
    app.needs_redraw = true;
}

pub(crate) fn close(app: &mut App) {
    app.gateway_view = None;
    app.needs_redraw = true;
}

/// Handle a key while the view is open. Always consumes the key
/// (returns `true`; the overlay is modal). `esc` closes; every other
/// key is inert - this view inspects, it never acts.
pub(crate) fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    if app.gateway_view.is_none() {
        return false;
    }
    if key.code == KeyCode::Esc {
        close(app);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn org(name: &str) -> GatewayOrgView {
        GatewayOrgView {
            org: name.to_owned(),
            accounts: vec!["A".to_owned()],
            fallback_accounts: Vec::new(),
            rows: Vec::new(),
        }
    }

    #[test]
    fn esc_closes_the_view() {
        let mut app = App::test_default();
        open(&mut app, vec![org("Default")]);
        assert!(app.gateway_view.is_some());
        assert!(handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(app.gateway_view.is_none(), "esc closes the gateway view");
    }

    #[test]
    fn other_keys_are_consumed_without_acting() {
        let mut app = App::test_default();
        open(&mut app, vec![org("Default"), org("Other")]);
        for code in [KeyCode::Enter, KeyCode::Char('q'), KeyCode::Down] {
            assert!(handle_key(&mut app, KeyEvent::new(code, KeyModifiers::NONE)));
            assert!(app.gateway_view.is_some(), "the read-only view never acts on a key");
        }
        assert_eq!(
            app.gateway_view.as_ref().map(|state| state.orgs.len()),
            Some(2),
            "the snapshot is untouched by navigation keys",
        );
    }

    #[test]
    fn handle_key_is_inert_while_closed() {
        let mut app = App::test_default();
        assert!(!handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    }
}
