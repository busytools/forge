//! forge-tui binary — terminal client entrypoint.
//!
//! Connects to forge-daemon, drives the screen-based app loop. Notifies
//! the loop of WS connection state, daemon notifications, reverse-RPC
//! requests, and terminal events.

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
    /// forge-daemon URL.
    #[arg(long, default_value = "ws://127.0.0.1:7373/")]
    forged: String,

    /// Connection name advertised to the daemon (shown in `session.peers`).
    #[arg(long, default_value = "forge-tui")]
    name: String,
}

/// RAII guard that flips the terminal back to cooked mode + main screen
/// even on panic.
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
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("forge_tui=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    let separator = if cli.forged.contains('?') { '&' } else { '?' };
    let encoded = urlencode_minimal(&cli.name);
    let url = format!("{}{}name={}", cli.forged, separator, encoded);
    let daemon_url = cli.forged.clone();
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_default();

    tracing::info!(url = %url, "connecting to forge-daemon");
    let client = Arc::new(Client::connect(&url).await?);
    tracing::info!("connected; entering app loop");

    let (event_tx, event_rx) = mpsc::unbounded_channel::<AppEvent>();

    // We're connected by the time `Client::connect` returns.
    let _ = event_tx.send(AppEvent::Connected);

    register_reverse_rpc_handlers(&client, &event_tx);
    spawn_initial_session_list_load(&client, &event_tx);
    spawn_notification_router(&client, &event_tx);

    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    spawn_terminal_event_pump(&event_tx);

    let app_state = app::App::new(daemon_url, cwd);
    let result = app::run(&mut terminal, client, app_state, event_tx, event_rx).await;

    drop(terminal);
    result?;
    Ok(())
}

fn register_reverse_rpc_handlers(
    client: &Arc<Client>,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
) {
    // permission.request — deferred: app loop answers via send_response
    // when the user picks Allow/Deny.
    {
        let tx = event_tx.clone();
        client.on_reverse_rpc_deferred(
            "permission.request",
            move |rev_id: serde_json::Value, params: serde_json::Value| {
                let tx = tx.clone();
                async move {
                    if tx
                        .send(AppEvent::PermissionRequest { rev_id, params })
                        .is_err()
                    {
                        tracing::warn!(
                            "permission.request received but app channel closed; \
                             daemon will time out"
                        );
                    }
                }
            },
        );
    }

    // Auto-allow hooks for v1 with passthrough decision. Interactive
    // hook approval is a Phase 4 follow-up.
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
            serde_json::json!({"decision": "passthrough"})
        });
    }
}

fn spawn_initial_session_list_load(
    client: &Arc<Client>,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
) {
    let client = client.clone();
    let tx = event_tx.clone();
    tokio::spawn(async move {
        let result: Result<serde_json::Value, _> = client
            .call(
                "sessions.list",
                serde_json::json!({"directory": std::env::current_dir().ok().and_then(|p| p.to_str().map(String::from))}),
            )
            .await;
        match result {
            Ok(v) => {
                let items = v
                    .get("sessions")
                    .and_then(|s| s.as_array())
                    .cloned()
                    .unwrap_or_default();
                let _ = tx.send(AppEvent::SessionListLoaded(items));
            }
            Err(e) => {
                let _ = tx.send(AppEvent::SessionListLoadFailed(e.to_string()));
            }
        }
    });
}

fn spawn_notification_router(client: &Arc<Client>, event_tx: &mpsc::UnboundedSender<AppEvent>) {
    let Some(mut notifications) = client.notifications() else {
        return;
    };
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

fn spawn_terminal_event_pump(event_tx: &mpsc::UnboundedSender<AppEvent>) {
    let term_tx = event_tx.clone();
    tokio::spawn(async move {
        use futures_util::StreamExt;
        let mut events = crossterm::event::EventStream::new();
        while let Some(item) = events.next().await {
            match item {
                Ok(e) => {
                    if term_tx.send(AppEvent::Term(e)).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "crossterm event stream Err; closing input pump");
                    break;
                }
            }
        }
    });
}

/// Minimal `application/x-www-form-urlencoded` encoder for the `?name=`
/// query parameter. Daemon-side `parse_query` reverses this.
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
    fn ampersand_and_equals_get_encoded() {
        assert_eq!(urlencode_minimal("a=b&c"), "a%3Db%26c");
    }
}
