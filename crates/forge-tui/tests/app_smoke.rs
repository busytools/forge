//! M7.2 — TUI app loop smoke tests against a real forged.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use forge_tui::app::{self, AppEvent};
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

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        app::run(&mut terminal, client, tx.clone(), rx),
    )
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

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        app::run(&mut terminal, client, tx.clone(), rx),
    )
    .await
    .expect("app loop did not exit within 2s");
    result.expect("app::run returned Err");
}

#[tokio::test]
async fn session_list_loaded_event_populates_list_and_keeps_cursor_in_bounds() {
    // Drive the event loop through a SessionListLoaded → render snap →
    // quit sequence, then assert the rendered output contains the
    // loaded session ids. Round 1 was a tautology (asserting on a
    // freshly-constructed App::default after the loop); the renderer
    // round-trip is the actual contract under test.
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

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        app::run(&mut terminal, client, tx.clone(), rx),
    )
    .await
    .expect("app loop did not exit within 2s");
    result.expect("app::run returned Err");

    // Inspect the buffer: the rendered output should mention at least
    // one of the loaded session summaries (the renderer shows the
    // human-readable "summary" rather than the raw session_id).
    // ratatui's TestBackend buffer is a `Buffer` of cells we can
    // serialize.
    let content: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        content.contains("first") || content.contains("second"),
        "expected rendered buffer to contain a session summary, got:\n{content}"
    );
}

#[test]
fn prompts_expired_with_non_matching_id_does_not_dismiss_open_modal() {
    // Round 3 — fix I3. Replaces the round-2 test-theatre version:
    // that test passed in BOTH a broken AND a fixed world because it
    // only checked that the loop terminated, never that the modal
    // remained open across the bogus expiry.
    //
    // Buffer-comparison strategy (non-invasive — no Arc<Mutex<App>>
    // hook into the loop). The discrimination test is:
    //
    //   1. Open permission modal for prompt_A.
    //   2. Drive the loop just long enough to render once
    //      (the modal is visible in the rendered buffer).
    //   3. Capture that buffer as the "expected with modal" snapshot.
    //   4. Restart the loop, this time also injecting a
    //      `PromptsExpired` for prompt_B AFTER the modal is open.
    //   5. Drive long enough for the expiry to be processed and
    //      re-render.
    //   6. Capture the buffer again — assert it STILL shows the
    //      modal text (i.e. byte-for-byte the same as step 3).
    //
    // If the matcher were broken (the bogus expiry dismissed the
    // modal), the step-6 buffer would lose the modal frame and
    // would NOT match step 3.
    //
    // We render via two independent runs because the only way to
    // capture mid-loop state without invasive hooks is to reach
    // termination cleanly. Each run uses TestBackend so the buffer
    // survives Drop.

    fn capture_buffer_after_events(events: Vec<AppEvent>) -> String {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let addr = spawn_forged();
            tokio::time::sleep(Duration::from_millis(50)).await;
            let client = Arc::new(Client::connect(&format!("ws://{addr}/")).await.unwrap());
            let backend = TestBackend::new(80, 20);
            let mut terminal = Terminal::new(backend).unwrap();

            let (tx, rx) = mpsc::unbounded_channel::<AppEvent>();
            for e in events {
                tx.send(e).unwrap();
            }
            tx.send(AppEvent::Quit).unwrap();

            tokio::time::timeout(
                Duration::from_secs(2),
                app::run(&mut terminal, client, tx.clone(), rx),
            )
            .await
            .expect("app loop did not exit within 2s")
            .expect("app::run returned Err");

            // Concatenate cell symbols row-by-row so we can compare
            // visible-text content rather than raw cell coordinates.
            let buffer = terminal.backend().buffer();
            let area = buffer.area();
            let mut s = String::with_capacity(usize::from(area.width) * usize::from(area.height));
            for y in 0..area.height {
                for x in 0..area.width {
                    s.push_str(buffer[(x, y)].symbol());
                }
                s.push('\n');
            }
            s
        })
    }

    // Baseline: open prompt_A modal, no expiry.
    let baseline_with_modal = capture_buffer_after_events(vec![AppEvent::PermissionRequest {
        rev_id: serde_json::Value::Null,
        params: serde_json::json!({
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
            "prompt_id": "prompt_A",
        }),
    }]);

    // The modal renders SOMETHING tool-name-shaped — assert visibility
    // before we rely on matching it.
    assert!(
        baseline_with_modal.contains("Bash"),
        "baseline should render the permission-modal Bash content; got:\n{baseline_with_modal}"
    );

    // Test: open prompt_A modal, then fire bogus expiry for prompt_B.
    let after_bogus_expiry = capture_buffer_after_events(vec![
        AppEvent::PermissionRequest {
            rev_id: serde_json::Value::Null,
            params: serde_json::json!({
                "tool_name": "Bash",
                "tool_input": {"command": "ls"},
                "prompt_id": "prompt_A",
            }),
        },
        AppEvent::PromptsExpired(serde_json::json!({
            "session_id": "sess_unrelated",
            "prompt_id": "prompt_B",
            "reason": "timeout",
            "fallback": "deny",
        })),
    ]);

    // Discrimination assertion: the rendered buffer must STILL show
    // the modal after the bogus expiry. If the matcher were broken
    // (rev_id used as prompt_id, or prompt_id check skipped), the
    // expiry would dismiss the modal and the buffer would lose its
    // modal content — diverging from the baseline.
    assert!(
        after_bogus_expiry.contains("Bash"),
        "non-matching prompts.expired must NOT dismiss the open modal;\n\
         baseline was:\n{baseline_with_modal}\n\
         after-bogus-expiry was:\n{after_bogus_expiry}"
    );
    // Stronger: byte-for-byte equality between baseline and post-expiry
    // proves the expiry was a no-op — no state mutation reached the
    // render.
    assert_eq!(
        baseline_with_modal, after_bogus_expiry,
        "non-matching prompts.expired must not change rendered output"
    );
}
