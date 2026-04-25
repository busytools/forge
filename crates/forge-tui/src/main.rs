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
use tracing_subscriber::EnvFilter;

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
#[allow(
    clippy::too_many_lines,
    reason = "binary main wires up clients/handlers and notifications"
)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Init tracing to stderr — the TUI owns stdout (alternate screen),
    // so logs go to stderr where they don't corrupt the rendered UI.
    // Honour `RUST_LOG`; default to `forge_tui=info` for ergonomic
    // visibility into client + dispatch errors.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("forge_tui=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    // Append `?name=...` (or `&name=...` if there's already a query string).
    // URL-encode the name so values with spaces / unicode (e.g.
    // "studio terminal", "café") produce a valid URL — the daemon's
    // `parse_query` reverses this on the other side.
    let separator = if cli.forged.contains('?') { '&' } else { '?' };
    let encoded = urlencode_minimal(&cli.name);
    let url = format!("{}{}name={}", cli.forged, separator, encoded);

    let client = Arc::new(Client::connect(&url).await?);

    let (event_tx, event_rx) = mpsc::unbounded_channel::<AppEvent>();

    // Register reverse-RPC handler for permission requests. The handler
    // captures the rev_id alongside the params and forwards both to the
    // app event channel; `input::answer_permission` later replies with
    // `Client::send_response(rev_id, ...)`. Registered DEFERRED — the
    // dispatcher does not auto-reply; the user keypress does.
    {
        let tx = event_tx.clone();
        client.on_reverse_rpc_deferred(
            "permission.request",
            move |rev_id: serde_json::Value, params: serde_json::Value| {
                let tx = tx.clone();
                async move {
                    let _ = tx.send(AppEvent::PermissionRequest { rev_id, params });
                }
            },
        );
    }

    // Auto-allow hooks for v1. Aligned with `attach_hooks` in
    // `crates/forged/src/sdk_callbacks.rs` — every kind the daemon
    // recognises must have a matching handler here, and we drop kinds
    // the daemon does not recognise (`session_start`, `session_end`).
    // Registered SYNC so the dispatcher auto-replies with the
    // passthrough decision. No AppEvent is forwarded — the app does
    // not need to know about auto-allowed hooks; future iterations
    // will surface them via a separate UI event when interactive
    // approval lands.
    let known_hook_kinds = [
        "pre_tool_use",
        "post_tool_use",
        "post_tool_use_failure",
        "user_prompt_submit",
        "stop",
        "subagent_stop",
        "subagent_start",
        "pre_compact",
        "notification",
        "permission_request",
    ];
    for kind in known_hook_kinds {
        let method = format!("hook.{kind}");
        client.on_reverse_rpc_sync(method, move |_rev_id, _params| async move {
            // Auto-allow shape: passthrough decision so the
            // forged-side bridge maps to `HookDecision::passthrough`
            // and the SDK reads it as "no opinion, proceed".
            serde_json::json!({"decision": "passthrough"})
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

    // Drain Client-wide notifications (everything that isn't a
    // session.event subscription notification or a reverse-RPC) and
    // route into the right AppEvent variants.
    if let Some(mut notifications) = client.notifications() {
        let tx = event_tx.clone();
        tokio::spawn(async move {
            while let Some(frame) = notifications.recv().await {
                let event = match frame.method.as_str() {
                    "session.role_assigned" => AppEvent::RoleChanged(frame.params),
                    "session.primary_changed" => AppEvent::PrimaryChanged(frame.params),
                    "session.closed" => AppEvent::SessionClosed(frame.params),
                    "prompts.expired" => AppEvent::PromptsExpired(frame.params),
                    other => {
                        tracing::debug!(method = %other, "unrouted notification");
                        continue;
                    }
                };
                if tx.send(event).is_err() {
                    break;
                }
            }
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

    let result = app::run(&mut terminal, client, event_tx, event_rx).await;

    // _guard is dropped here, restoring the terminal automatically.
    drop(terminal);

    result?;
    Ok(())
}

/// Minimal `application/x-www-form-urlencoded` encoder for the `?name=`
/// query parameter. Walks the input as bytes and emits `%XX` for any
/// byte outside the unreserved set (`A-Z`, `a-z`, `0-9`, `_`, `.`, `-`)
/// — that matches the daemon's `url_decode` inverse so a name like
/// "studio terminal" or "café" round-trips losslessly.
fn urlencode_minimal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_nibble(b >> 4));
            out.push(hex_nibble(b & 0x0f));
        }
    }
    out
}

/// Render a 4-bit value as an uppercase hex digit (`0`-`9`, `A`-`F`).
fn hex_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'A' + (n - 10)) as char,
        _ => '?',
    }
}

#[cfg(test)]
mod tests {
    use super::urlencode_minimal;

    #[test]
    fn unreserved_bytes_pass_through_unchanged() {
        assert_eq!(urlencode_minimal("forge-tui_v1.0"), "forge-tui_v1.0");
    }

    #[test]
    fn space_becomes_percent_20() {
        assert_eq!(urlencode_minimal("studio terminal"), "studio%20terminal");
    }

    #[test]
    fn multibyte_utf8_encodes_byte_by_byte() {
        // "é" is 0xC3 0xA9 in UTF-8.
        assert_eq!(urlencode_minimal("café"), "caf%C3%A9");
    }

    #[test]
    fn ampersand_and_equals_get_encoded() {
        assert_eq!(urlencode_minimal("a=b&c"), "a%3Db%26c");
    }
}
