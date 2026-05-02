use forge_tui::agent::events::ClientEvent;
use forge_tui::app::App;

/// Build a minimal `App` for in-process integration-style testing.
/// This exercises app state and event handling directly, without a real bridge or TUI boundary.
pub fn test_app() -> App {
    App::test_default()
}

/// Send a client event through the app's in-process event handling pipeline.
pub fn send_client_event(app: &mut App, event: ClientEvent) {
    forge_tui::app::handle_client_event(app, event);
}
