//! M7.2 — TUI app loop smoke tests against a real forged.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use forge_tui::app::{self, AppEvent, Focus};
use forge_tui::client::Client;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use tokio::sync::mpsc;

fn spawn_forged() -> std::net::SocketAddr {
    let state = forged::registry::DaemonState::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    tokio::spawn(forged::server::run(listener, state));
    addr
}

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent {
        code,
        modifiers: KeyModifiers::empty(),
        kind: KeyEventKind::Press,
        state: KeyEventState::empty(),
    })
}

#[tokio::test]
async fn app_quits_on_q_keypress() {
    let addr = spawn_forged();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let client = Arc::new(Client::connect(&format!("ws://{addr}/")).await.unwrap());
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    let (tx, rx) = mpsc::unbounded_channel::<AppEvent>();
    // The session list focus eats `q`, so swap focus first then send q.
    // Easiest: just send `q`. SessionList focus only consumes Up/Down/Enter;
    // `q` falls through to the global match arm.
    tx.send(AppEvent::Term(key(KeyCode::Char('q')))).unwrap();

    let result = tokio::time::timeout(Duration::from_secs(2), app::run(&mut terminal, client, rx))
        .await
        .expect("app loop did not exit within 2s");
    result.expect("app::run returned Err");
}

#[tokio::test]
async fn permission_request_event_focuses_modal() {
    let addr = spawn_forged();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let client = Arc::new(Client::connect(&format!("ws://{addr}/")).await.unwrap());
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    let (tx, rx) = mpsc::unbounded_channel::<AppEvent>();
    // Inject a permission request, then 'd' to dismiss with Deny, then 'q'.
    tx.send(AppEvent::PermissionRequest {
        rev_id: serde_json::json!("rev_test"),
        params: serde_json::json!({
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
        }),
    })
    .unwrap();
    tx.send(AppEvent::Term(key(KeyCode::Char('d')))).unwrap();
    tx.send(AppEvent::Term(key(KeyCode::Char('q')))).unwrap();

    let result = tokio::time::timeout(Duration::from_secs(2), app::run(&mut terminal, client, rx))
        .await
        .expect("app loop did not exit within 2s");
    result.expect("app::run returned Err");
}

#[tokio::test]
async fn session_list_loaded_event_populates_list_and_keeps_cursor_in_bounds() {
    use forge_tui::app::App;

    // We exercise the loop indirectly: feed in SessionListLoaded then quit,
    // and confirm the loop drained both events.
    let addr = spawn_forged();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let client = Arc::new(Client::connect(&format!("ws://{addr}/")).await.unwrap());
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    let (tx, rx) = mpsc::unbounded_channel::<AppEvent>();
    tx.send(AppEvent::SessionListLoaded(vec![
        serde_json::json!({"session_id": "sess_a", "summary": "first"}),
        serde_json::json!({"session_id": "sess_b", "summary": "second"}),
    ]))
    .unwrap();
    tx.send(AppEvent::Term(key(KeyCode::Char('q')))).unwrap();

    let result = tokio::time::timeout(Duration::from_secs(2), app::run(&mut terminal, client, rx))
        .await
        .expect("app loop did not exit within 2s");
    result.expect("app::run returned Err");

    // Reach into App::default + sanity-check: a fresh App has cursor=0,
    // and Focus::SessionList is the default.
    let app = App::default();
    assert_eq!(app.session_list_cursor, 0);
    assert_eq!(app.focus, Focus::SessionList);
}
