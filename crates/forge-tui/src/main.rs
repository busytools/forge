//! forge-tui binary — terminal client entrypoint.
//!
//! Wires the WS+JSON-RPC client up to the TUI app loop:
//! - Connects to forged at `--forged` (default `ws://127.0.0.1:7373/`).
//! - Loads the session list once.
//! - Pumps terminal events into the app channel.
//! - Registers reverse-RPC handlers that forward `permission.request` to
//!   the app channel along with the JSON-RPC id; the keypress handler
//!   answers via [`forge_tui::client::Client::send_response`] when the
//!   user picks Allow/Deny.
//! - Restores the terminal on exit (raw mode + alternate screen).

#![allow(clippy::print_stdout, reason = "binary may print at top level")]

use std::io::stdout;
use std::sync::Arc;

use clap::Parser;
use crossterm::ExecutableCommand;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use forge_tui::app::{self, AppEvent};
use forge_tui::client::Client;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(name = "forge-tui", version, about = "forge terminal client")]
struct Cli {
    /// forged URL — `ws://127.0.0.1:7373/` or `wss://forged.example.com/`.
    #[arg(long, default_value = "ws://127.0.0.1:7373/")]
    forged: String,

    /// Connection name advertised to forged (shown in `session.peers`).
    #[arg(long, default_value = "forge-tui")]
    name: String,
}

/// RAII guard that flips the terminal back to cooked mode + main screen
/// even on panic, so a crash in the app loop doesn't leave the user's
/// terminal in alternate-screen mode.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> std::io::Result<Self> {
        enable_raw_mode()?;
        stdout().execute(EnterAlternateScreen)?;
        stdout().execute(EnableMouseCapture)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = stdout().execute(DisableMouseCapture);
        let _ = stdout().execute(LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Append `?name=...` (or `&name=...` if there's already a query string).
    let separator = if cli.forged.contains('?') { '&' } else { '?' };
    let url = format!("{}{}name={}", cli.forged, separator, cli.name);

    let client = Arc::new(Client::connect(&url).await?);

    let (event_tx, event_rx) = mpsc::unbounded_channel::<AppEvent>();

    // Register reverse-RPC handler for permission requests. The handler
    // captures the rev_id alongside the params and forwards both to the
    // app event channel; `input::answer_permission` later replies with
    // `Client::send_response(rev_id, ...)`.
    {
        let tx = event_tx.clone();
        client.on_reverse_rpc(
            "permission.request",
            move |rev_id: serde_json::Value, params: serde_json::Value| {
                let tx = tx.clone();
                async move {
                    let _ = tx.send(AppEvent::PermissionRequest { rev_id, params });
                    // Returned future value is ignored — the answer flows back
                    // out-of-band via Client::send_response.
                    serde_json::Value::Null
                }
            },
        );
    }

    // Auto-allow hooks for v1. Hooks signal the daemon they may proceed.
    let common_hook_methods = [
        "hook.pre_tool_use",
        "hook.post_tool_use",
        "hook.user_prompt_submit",
        "hook.stop",
        "hook.subagent_stop",
        "hook.pre_compact",
        "hook.session_start",
        "hook.session_end",
        "hook.notification",
    ];
    for method in common_hook_methods {
        let tx = event_tx.clone();
        let kind = method.trim_start_matches("hook.").to_string();
        client.on_reverse_rpc(method, move |rev_id, params| {
            let tx = tx.clone();
            let kind = kind.clone();
            async move {
                let _ = tx.send(AppEvent::HookRequest {
                    kind,
                    rev_id,
                    params,
                });
                // Auto-allow shape: matches forged's hook callback contract
                // (return an empty `decisions` array — see wire spec).
                serde_json::json!({"decisions": []})
            }
        });
    }

    // Initial session list load.
    {
        let client = client.clone();
        let tx = event_tx.clone();
        tokio::spawn(async move {
            let result: Result<serde_json::Value, _> =
                client.call("sessions.list", serde_json::json!({})).await;
            let items = match result {
                Ok(v) => v
                    .get("sessions")
                    .and_then(|s| s.as_array())
                    .cloned()
                    .unwrap_or_default(),
                Err(_) => Vec::new(),
            };
            let _ = tx.send(AppEvent::SessionListLoaded(items));
        });
    }

    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    // Pump terminal events. Spawning this AFTER `TerminalGuard::enter()`
    // ensures crossterm's event reader sees the configured TTY rather
    // than panicking on `reader source not set` when stdin is redirected.
    let term_tx = event_tx.clone();
    tokio::spawn(async move {
        use futures_util::StreamExt;
        let mut events = crossterm::event::EventStream::new();
        while let Some(Ok(e)) = events.next().await {
            if term_tx.send(AppEvent::Term(e)).is_err() {
                break;
            }
        }
    });

    let result = app::run(&mut terminal, client, event_rx).await;

    // _guard is dropped here, restoring the terminal automatically.
    drop(terminal);

    result?;
    Ok(())
}
