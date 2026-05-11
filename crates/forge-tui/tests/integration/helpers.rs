use forge_tui::app::App;
use forge_workspace::{SessionKey, SessionUpdate};

/// Build a minimal `App` for in-process integration-style testing.
/// This exercises app state and event handling directly, without a real bridge or TUI boundary.
pub fn test_app() -> App {
    App::test_default()
}

/// Send a `SessionUpdate` through the app's in-process event handling pipeline.
pub fn send_client_event(app: &mut App, event: SessionUpdate) {
    forge_tui::app::apply_session_update(app, event);
}

/// Borrow the currently-active [`SessionKey`] from the app, for
/// tagging synthetic [`SessionUpdate`]s emitted by integration tests.
/// Falls back to a deterministic test sentinel when the test app
/// hasn't seeded an active session yet.
pub fn active_session_key(app: &App) -> SessionKey {
    app.active_session_key
        .clone()
        .unwrap_or_else(|| SessionKey::from_str_for_test("__test_pre_connect__"))
}
