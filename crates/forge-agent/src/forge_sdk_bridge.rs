//! `ForgeSdkBridge` — in-process driver around a [`forge_sdk::Client`].
//!
//! Drives a [`forge_sdk::Client`] directly — no Node.js subprocess,
//! no NDJSON, no command queue. The bridge holds the spawned
//! `Arc<Client>` and dispatches each method as a direct call
//! (or a `tokio::spawn`'d async task when the method is
//! fire-and-forget). Synthesized events (Connected, `PermissionRequest`,
//! `McpSnapshot`, …) flow back through an `mpsc::UnboundedSender<AgentEvent>`
//! the bridge owns; consumers grab the matching receiver once via
//! [`ForgeSdkBridge::take_events`].
//!
//! ```text
//!     TUI                     ForgeSdkBridge                  forge_sdk::Client
//!      | trait method            |                                    |
//!      |------------------------>|  client.method().await             |
//!      |                         |----------------------------------->|
//!      |                         |                                    |
//!      |       AgentEvent        |  reader_loop / callbacks           |
//!      |<------ event_tx --------+<-----------------------------------|
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use forge_sdk::Client;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::client::{AgentEvent, SessionLaunchSettings};
use crate::forge_sdk_worker;
use forge_primitives::{ElicitationAction, McpServerConfig, PermissionOutcome, QuestionOutcome};

/// Sentinel `config_dir` for `ForgeSdkBridge` test stubs that never
/// exercise the path. Production code constructs the bridge with a
/// real account `config_dir`; tests that don't drive a session use
/// this path so the typed field stays non-optional.
const TESTING_STUB_CONFIG_DIR: &str = "/tmp/forge-testing-stub";

/// Pending permission responses keyed by `tool_use_id`. The
/// `can_use_tool` callback parks a oneshot here when the CLI asks;
/// dispatch drains it when the matching `permission_response` arrives
/// from the App.
pub(crate) type PendingResponses =
    Arc<Mutex<HashMap<String, oneshot::Sender<forge_sdk::PermissionDecision>>>>;

/// Pending question outcomes keyed by `tool_use_id`. The
/// `AskUserQuestion` driver in the `can_use_tool` callback parks a
/// fresh oneshot per question, emits a `QuestionRequest`, and awaits
/// the matching `question_response`.
pub(crate) type PendingQuestions = Arc<Mutex<HashMap<String, oneshot::Sender<QuestionOutcome>>>>;

/// In-process bridge wrapping a single [`forge_sdk::Client`].
///
/// Single instance per connection. The bridge owns the spawned
/// `forge_sdk::Client`, the `can_use_tool` parking lots, the per-cwd
/// git-context watchers, and the outbound `AgentEvent` channel.
#[derive(Clone)]
pub struct ForgeSdkBridge {
    inner: Arc<BridgeInner>,
}

pub(crate) struct BridgeInner {
    /// Set after first `new_session` / `resume_session`; cleared on
    /// session replace or shutdown.
    client: Mutex<Option<Client>>,
    /// Bridge → App event emission channel. Cloned freely into the
    /// reader subtask + `can_use_tool` callback closures.
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    /// Single-take receiver handed out via [`ForgeSdkBridge::take_events`].
    events_rx: Mutex<Option<mpsc::UnboundedReceiver<AgentEvent>>>,
    /// Permission round-trip parking lot.
    pub(crate) pending: PendingResponses,
    /// Question round-trip parking lot.
    pub(crate) pending_questions: PendingQuestions,
    /// Active git-context watcher tasks, keyed by `session_id`. Aborted
    /// on bridge drop or session replace.
    git_watchers: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// Current session id, shared with the `can_use_tool` callback so
    /// permission/question events carry the right `session_id`.
    pub(crate) session_id_slot: Arc<Mutex<String>>,
    /// `<config_dir>` this bridge was bound to at construction time
    /// (workspace-driven — typically the picked account's
    /// `config_dir`). Threaded into the spawned `claude`
    /// subprocess as `CLAUDE_CONFIG_DIR` and consulted by every
    /// in-process accessor (oauth, settings, catalog) so they
    /// honour the bound account, not whatever `$CLAUDE_CONFIG_DIR`
    /// the parent shell happened to have.
    config_dir: PathBuf,
    /// Forge-internal account display name from `forge.toml`'s
    /// `[[accounts]]`, set by the workspace picker. `None` when
    /// `Agent::spawn` is called directly (tests, smoke). When
    /// present, surfaced via [`AgentEvent::StatusSnapshot`] so the
    /// TUI can render which forge-account the bridge is bound to.
    display_name: Option<String>,
}

impl ForgeSdkBridge {
    /// Construct a fresh bridge bound to `config_dir` with an
    /// optional forge-account `display_name`. Both are stored as
    /// typed fields. `config_dir` is consulted by every in-process
    /// accessor (oauth, settings, catalog scans) and exported to
    /// the spawned `claude` subprocess as `CLAUDE_CONFIG_DIR`.
    /// `display_name`, when present, is surfaced via
    /// [`AgentEvent::StatusSnapshot`]. The internal event channel
    /// is created here; consumers grab the receiver once via
    /// [`ForgeSdkBridge::take_events`].
    #[must_use]
    pub(crate) fn new(config_dir: PathBuf, display_name: Option<String>) -> Self {
        let (event_tx, events_rx) = mpsc::unbounded_channel();
        Self {
            inner: Arc::new(BridgeInner {
                client: Mutex::new(None),
                event_tx,
                events_rx: Mutex::new(Some(events_rx)),
                pending: Arc::new(Mutex::new(HashMap::new())),
                pending_questions: Arc::new(Mutex::new(HashMap::new())),
                git_watchers: Mutex::new(HashMap::new()),
                session_id_slot: Arc::new(Mutex::new(String::new())),
                config_dir,
                display_name,
            }),
        }
    }

    pub(crate) fn event_tx(&self) -> &mpsc::UnboundedSender<AgentEvent> {
        &self.inner.event_tx
    }

    pub(crate) fn inner_pending(&self) -> &PendingResponses {
        &self.inner.pending
    }

    pub(crate) fn inner_pending_questions(&self) -> &PendingQuestions {
        &self.inner.pending_questions
    }

    pub(crate) fn session_id_slot_arc(&self) -> &Arc<Mutex<String>> {
        &self.inner.session_id_slot
    }

    fn client(&self) -> Option<Client> {
        self.inner.client.lock().clone()
    }

    pub(crate) fn set_client(&self, client: Client) {
        *self.inner.client.lock() = Some(client);
    }

    pub(crate) fn clear_client(&self) -> Option<Client> {
        self.inner.client.lock().take()
    }

    /// Send an `AgentEvent::McpOperationError` to the App; log a
    /// `BRIDGE_LIFECYCLE` warn if the channel is closed (terminal
    /// event for a user-visible MCP failure — silent-drop on
    /// teardown race would lose the only path to surface).
    /// On send failure we destructure the unsent event back out of
    /// the `SendError` so the warn log carries `session_id`,
    /// `server_name`, and the underlying error text — the most
    /// actionable triage signals.
    fn emit_mcp_error_or_log(
        event_tx: &mpsc::UnboundedSender<AgentEvent>,
        session_id: String,
        operation: &'static str,
        server_name: Option<String>,
        error_msg: String,
    ) {
        let event = AgentEvent::McpOperationError {
            session_id,
            error: forge_primitives::McpOperationError {
                operation: operation.to_owned(),
                server_name,
                message: error_msg,
            },
        };
        if let Err(send_err) = event_tx.send(event) {
            // Unreachable in practice — we just constructed
            // McpOperationError on the line above and the SendError
            // wraps the unsent event verbatim.
            let AgentEvent::McpOperationError { session_id, error } = send_err.0 else {
                unreachable!("McpOperationError just constructed above")
            };
            tracing::warn!(
                target: crate::logging::targets::BRIDGE_LIFECYCLE,
                session_id = %session_id,
                operation,
                server_name = ?error.server_name,
                error_msg = %error.message,
                "event channel closed; McpOperationError dropped",
            );
        }
    }

    /// Spawn a fire-and-forget client call. Logs and drops on failure.
    fn dispatch<F, Fut>(&self, label: &'static str, f: F) -> anyhow::Result<()>
    where
        F: FnOnce(Client) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let Some(client) = self.client() else {
            return Err(anyhow::anyhow!("forge-sdk bridge: {label} called before active session"));
        };
        tokio::spawn(async move {
            if let Err(err) = f(client).await {
                tracing::warn!(
                    target: crate::logging::targets::BRIDGE_LIFECYCLE,
                    label,
                    error = %err,
                    "forge-sdk bridge: dispatch failed",
                );
            }
        });
        Ok(())
    }

    /// Replace any existing git watcher for `session_id` with a new
    /// task that pumps `GitContextWatcher` snapshots into the event
    /// channel.
    fn install_git_watcher(&self, session_id: String, cwd: &Path) {
        // Abort any prior watcher for this session so notify cleans up
        // its OS-level subscriptions before we replace it.
        if let Some(prev) = self.inner.git_watchers.lock().remove(&session_id) {
            prev.abort();
        }

        let mut watcher = match crate::env::git::GitContextWatcher::new(cwd) {
            Ok(watcher) => watcher,
            Err(err) => {
                tracing::warn!(
                    target: crate::logging::targets::BRIDGE_LIFECYCLE,
                    session_id = %session_id,
                    cwd = %cwd.display(),
                    error = %err,
                    "failed to start git context watcher",
                );
                return;
            }
        };
        let event_tx = self.inner.event_tx.clone();
        let task_session_id = session_id.clone();
        let handle = tokio::spawn(async move {
            while let Some(context) = watcher.next_snapshot().await {
                if event_tx
                    .send(AgentEvent::GitContextSnapshot {
                        session_id: task_session_id.clone(),
                        context,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        self.inner.git_watchers.lock().insert(session_id, handle);
    }

    fn stop_git_watcher(&self, session_id: &str) {
        if let Some(handle) = self.inner.git_watchers.lock().remove(session_id) {
            handle.abort();
        }
    }
}

impl Default for ForgeSdkBridge {
    fn default() -> Self {
        Self::new(PathBuf::from(TESTING_STUB_CONFIG_DIR), None)
    }
}

impl Drop for BridgeInner {
    fn drop(&mut self) {
        for (_, handle) in self.git_watchers.lock().drain() {
            handle.abort();
        }
    }
}

// `unused_self` / `needless_pass_by_value` are allowed at the impl
// level: passthrough accessors that delegate to `forge_sdk::*` free
// functions don't logically use `self`, but every method here
// preserves the `&self`-receiver shape so AgentHandle wrappers can
// call them through a single stable seam.
#[allow(clippy::unused_self, clippy::needless_pass_by_value)]
impl ForgeSdkBridge {
    pub(crate) fn take_events(&self) -> Option<mpsc::UnboundedReceiver<AgentEvent>> {
        self.inner.events_rx.lock().take()
    }

    pub(crate) fn prompt_text(&self, session_id: String, text: String) -> anyhow::Result<()> {
        self.prompt_with_images(session_id, text, Vec::new())
    }

    pub(crate) fn prompt_with_images(
        &self,
        session_id: String,
        text: String,
        images: Vec<forge_primitives::ImageAttachment>,
    ) -> anyhow::Result<()> {
        // No `check_session_id` here — the TUI commits to
        // `AppStatus::Thinking` BEFORE this call, and a silent
        // stale-session drop (returning Ok with no event) would
        // leave the user's spinner running forever with no
        // recovery path. If the prompt does land in the wrong
        // session, normal SDK error surfaces (TurnError, etc.)
        // tear down Thinking via the existing wire path. We still
        // log a breadcrumb on mismatch so the bypass is observable.
        self.trace_session_id_bypass(&session_id, "prompt_with_images");
        let mut chunks: Vec<forge_primitives::PromptChunk> = Vec::with_capacity(1 + images.len());
        for img in images {
            if let Err(reason) = forge_primitives::validate_image(&img.data, &img.mime_type) {
                tracing::warn!(
                    target: crate::logging::targets::APP_INPUT,
                    "forge-sdk bridge: skipping invalid image: {reason}"
                );
                continue;
            }
            chunks.push(forge_primitives::PromptChunk {
                kind: "image".to_owned(),
                value: serde_json::json!({
                    "data": img.data,
                    "mime_type": img.mime_type,
                }),
            });
        }
        chunks.push(forge_primitives::PromptChunk {
            kind: "text".to_owned(),
            value: Value::String(text),
        });
        self.dispatch("prompt", move |client| async move {
            forge_sdk_worker::send_prompt(&client, chunks).await
        })
    }

    pub(crate) fn cancel(&self, session_id: String) -> anyhow::Result<()> {
        if !self.check_session_id(&session_id, "cancel") {
            return Ok(());
        }
        self.dispatch("cancel", |client| async move {
            client.interrupt().await?;
            Ok(())
        })
    }

    pub(crate) fn set_mode(&self, session_id: String, mode: String) -> anyhow::Result<()> {
        if !self.check_session_id(&session_id, "set_mode") {
            return Ok(());
        }
        let parsed = forge_sdk_worker::parse_permission_mode(&mode)?;
        self.dispatch("set_mode", move |client| async move {
            client.set_permission_mode(parsed).await?;
            Ok(())
        })
    }

    pub(crate) fn set_model(&self, session_id: String, model: String) -> anyhow::Result<()> {
        if !self.check_session_id(&session_id, "set_model") {
            return Ok(());
        }
        self.dispatch("set_model", move |client| async move {
            client.set_model(Some(model.as_str())).await?;
            Ok(())
        })
    }

    /// Verify the requested `session_id` matches the bridge's current
    /// session before dispatching a user-action method. In a session-swap
    /// race a `cancel`/`set_mode`/`set_model` for session A could
    /// otherwise hit session B's `Client`. Emits a debug breadcrumb
    /// here on mismatch and returns false; the caller is expected to
    /// drop the dispatch with a no-op `Ok(())`.
    ///
    /// Other user-action methods (`prompt_with_images`,
    /// `generate_session_title`, `respond_to_elicitation`) intentionally
    /// opt out — see the inline rationale at each call site.
    fn check_session_id(&self, session_id: &str, label: &'static str) -> bool {
        let current = self.inner.session_id_slot.lock().clone();
        if current.is_empty() || current == session_id {
            return true;
        }
        tracing::debug!(
            target: crate::logging::targets::BRIDGE_LIFECYCLE,
            event_name = "stale_session_dispatch",
            label,
            current_session_id = %current,
            requested_session_id = %session_id,
            "dropping dispatch for stale session id"
        );
        false
    }

    /// Sibling of [`check_session_id`] for methods that intentionally
    /// pass through on mismatch (no silent drop). Logs a breadcrumb at
    /// `trace` so postmortems can correlate "my prompt vanished" with
    /// a session-swap race, without raising the noise floor under
    /// normal operation.
    fn trace_session_id_bypass(&self, session_id: &str, label: &'static str) {
        let current = self.inner.session_id_slot.lock().clone();
        if current.is_empty() || current == session_id {
            return;
        }
        tracing::trace!(
            target: crate::logging::targets::BRIDGE_LIFECYCLE,
            event_name = "stale_session_bypass",
            label,
            current_session_id = %current,
            requested_session_id = %session_id,
            "passing through dispatch despite stale session id (intentional bypass)",
        );
    }

    pub(crate) fn generate_session_title(
        &self,
        session_id: String,
        description: String,
    ) -> anyhow::Result<()> {
        // No `check_session_id` here — title generation is a
        // best-effort cosmetic update; even mis-routed it can't
        // wedge user-visible state. Trace breadcrumb keeps the
        // bypass observable.
        self.trace_session_id_bypass(&session_id, "generate_session_title");
        self.dispatch("generate_session_title", move |client| async move {
            let _ = client.generate_session_title(&description).await?;
            Ok(())
        })
    }

    pub(crate) fn rename_session(&self, session_id: String, title: String) -> anyhow::Result<()> {
        // Offline disk mutation — no Client required.
        crate::userdata::catalog::mutations::rename_session(
            &self.inner.config_dir,
            &session_id,
            &title,
            None,
        )?;
        Ok(())
    }

    pub(crate) fn get_status_snapshot(&self, session_id: String) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        let config_dir = self.inner.config_dir.clone();
        let display_name = self.inner.display_name.clone();
        self.dispatch("get_status_snapshot", move |client| async move {
            let account = client
                .account_info_from_init()
                .or_else(|| crate::cloud::auth_status::account_info_from_shell(&config_dir))
                .unwrap_or_default();
            let forge_account = display_name.map(forge_primitives::ForgeAccountIdentity::new);
            let _ =
                event_tx.send(AgentEvent::StatusSnapshot { session_id, account, forge_account });
            Ok(())
        })
    }

    pub(crate) fn get_oauth_credentials_snapshot(&self, session_id: String) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        let config_dir = self.inner.config_dir.clone();
        self.dispatch("get_oauth_credentials_snapshot", move |_client| async move {
            let credentials = crate::cloud::oauth_credentials::load_oauth_credentials(&config_dir);
            let _ = event_tx.send(AgentEvent::OauthCredentialsSnapshot { session_id, credentials });
            Ok(())
        })
    }

    pub(crate) fn get_context_usage(&self, session_id: String) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        self.dispatch("get_context_usage", move |client| async move {
            let usage = client.get_context_usage().await?;
            let percentage = forge_sdk_worker::clamp_percentage_to_u8(usage.percentage);
            let _ = event_tx
                .send(AgentEvent::ContextUsage { session_id, percentage: Some(percentage) });
            Ok(())
        })
    }

    pub(crate) fn reload_plugins(&self, session_id: String) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        self.dispatch("reload_plugins", move |client| async move {
            match client.reload_plugins().await {
                Ok(_) => {
                    let _ = event_tx.send(AgentEvent::RuntimeReloadCompleted { session_id });
                }
                Err(e) => {
                    let msg = format!("reload_plugins failed: {e}");
                    if event_tx
                        .send(AgentEvent::RuntimeReloadFailed { session_id, message: msg.clone() })
                        .is_err()
                    {
                        tracing::warn!(
                            target: crate::logging::targets::BRIDGE_LIFECYCLE,
                            error = %msg,
                            "event channel closed; RuntimeReloadFailed dropped",
                        );
                    }
                }
            }
            Ok(())
        })
    }

    pub(crate) fn get_mcp_snapshot(&self, session_id: String) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        self.dispatch("get_mcp_snapshot", move |client| async move {
            let response = client.mcp_status().await?;
            let _ = event_tx.send(AgentEvent::McpSnapshot {
                session_id,
                servers: response.mcp_servers,
                error: None,
            });
            Ok(())
        })
    }

    pub(crate) fn respond_to_elicitation(
        &self,
        session_id: String,
        elicitation_request_id: String,
        action: ElicitationAction,
        content: Option<Value>,
    ) -> anyhow::Result<()> {
        // No `check_session_id` — same shape as `prompt_with_images`:
        // an elicitation has its own request_id seam, and a silent
        // stale-session drop would leave the agent waiting forever
        // for a response that no longer comes.
        self.trace_session_id_bypass(&session_id, "respond_to_elicitation");
        let action_str = match action {
            ElicitationAction::Accept => "accept",
            ElicitationAction::Decline => "decline",
            ElicitationAction::Cancel => "cancel",
        };
        self.dispatch("respond_to_elicitation", move |client| async move {
            client.respond_to_elicitation(&elicitation_request_id, action_str, content).await?;
            Ok(())
        })
    }

    pub(crate) fn reconnect_mcp_server(
        &self,
        session_id: String,
        server_name: String,
    ) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        self.dispatch("reconnect_mcp_server", move |client| async move {
            if let Err(e) = client.mcp_reconnect(&server_name).await {
                Self::emit_mcp_error_or_log(
                    &event_tx,
                    session_id,
                    "reconnect",
                    Some(server_name),
                    format!("{e}"),
                );
            }
            Ok(())
        })
    }

    pub(crate) fn toggle_mcp_server(
        &self,
        session_id: String,
        server_name: String,
        enabled: bool,
    ) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        self.dispatch("toggle_mcp_server", move |client| async move {
            if let Err(e) = client.mcp_toggle(&server_name, enabled).await {
                Self::emit_mcp_error_or_log(
                    &event_tx,
                    session_id,
                    "toggle",
                    Some(server_name),
                    format!("{e}"),
                );
            }
            Ok(())
        })
    }

    pub(crate) fn set_mcp_servers(
        &self,
        session_id: String,
        servers: std::collections::BTreeMap<String, McpServerConfig>,
    ) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        let payload = serde_json::to_value(servers)?;
        self.dispatch("set_mcp_servers", move |client| async move {
            if let Err(e) = client.mcp_set_servers(payload).await {
                Self::emit_mcp_error_or_log(
                    &event_tx,
                    session_id,
                    "set_servers",
                    None,
                    format!("{e}"),
                );
            }
            Ok(())
        })
    }

    pub(crate) fn authenticate_mcp_server(
        &self,
        session_id: String,
        server_name: String,
    ) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        self.dispatch("authenticate_mcp_server", move |client| async move {
            match client.mcp_authenticate(&server_name).await {
                Ok(response) => {
                    let url = response
                        .get("redirect_url")
                        .or_else(|| response.get("authUrl"))
                        .or_else(|| response.get("auth_url"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    if let Some(auth_url) = url {
                        let _ = event_tx.send(AgentEvent::McpAuthRedirect {
                            session_id,
                            redirect: forge_primitives::McpAuthRedirect {
                                server_name,
                                auth_url,
                                requires_user_action: true,
                            },
                        });
                    } else {
                        // Without a redirect URL the TUI's authenticating
                        // overlay would hang forever — surface as an
                        // operation error so the user sees the failure.
                        Self::emit_mcp_error_or_log(
                            &event_tx,
                            session_id,
                            "authenticate",
                            Some(server_name),
                            "MCP authentication response had no redirect URL".to_owned(),
                        );
                    }
                }
                Err(e) => {
                    Self::emit_mcp_error_or_log(
                        &event_tx,
                        session_id,
                        "authenticate",
                        Some(server_name),
                        format!("{e}"),
                    );
                }
            }
            Ok(())
        })
    }

    pub(crate) fn clear_mcp_auth(
        &self,
        session_id: String,
        server_name: String,
    ) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        self.dispatch("clear_mcp_auth", move |client| async move {
            if let Err(e) = client.mcp_clear_auth(&server_name).await {
                Self::emit_mcp_error_or_log(
                    &event_tx,
                    session_id,
                    "clear_auth",
                    Some(server_name),
                    format!("{e}"),
                );
            }
            Ok(())
        })
    }

    pub(crate) fn submit_mcp_oauth_callback_url(
        &self,
        session_id: String,
        server_name: String,
        callback_url: String,
    ) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        self.dispatch("submit_mcp_oauth_callback_url", move |client| async move {
            if let Err(e) = client.mcp_oauth_callback_url(&server_name, &callback_url).await {
                Self::emit_mcp_error_or_log(
                    &event_tx,
                    session_id,
                    "oauth_callback",
                    Some(server_name),
                    format!("{e}"),
                );
            }
            Ok(())
        })
    }

    pub(crate) fn new_session(
        &self,
        cwd: String,
        launch_settings: SessionLaunchSettings,
    ) -> anyhow::Result<()> {
        let bridge = self.clone();
        tokio::spawn(async move {
            if let Err(err) =
                forge_sdk_worker::spawn_session(&bridge, &cwd, None, &launch_settings).await
            {
                let msg = format!("forge-sdk session spawn failed: {err}");
                if bridge
                    .event_tx()
                    .send(AgentEvent::ConnectionFailed { message: msg.clone() })
                    .is_err()
                {
                    tracing::warn!(
                        target: crate::logging::targets::BRIDGE_LIFECYCLE,
                        error = %msg,
                        "event channel closed; ConnectionFailed dropped",
                    );
                }
            }
        });
        Ok(())
    }

    pub(crate) fn resume_session(
        &self,
        session_id: String,
        cwd: String,
        launch_settings: SessionLaunchSettings,
    ) -> anyhow::Result<()> {
        let bridge = self.clone();
        tokio::spawn(async move {
            if let Err(err) =
                forge_sdk_worker::spawn_session(&bridge, &cwd, Some(&session_id), &launch_settings)
                    .await
            {
                let msg = format!("forge-sdk session resume failed: {err}");
                if bridge
                    .event_tx()
                    .send(AgentEvent::ConnectionFailed { message: msg.clone() })
                    .is_err()
                {
                    tracing::warn!(
                        target: crate::logging::targets::BRIDGE_LIFECYCLE,
                        error = %msg,
                        "event channel closed; ConnectionFailed dropped",
                    );
                }
            }
        });
        Ok(())
    }

    /// Try resume; on failure transparently retry as new_session(cwd).
    /// Surfaces `ConnectionFailed` only when both attempts fail. Used
    /// by project-rooted spawns (Default / Named) where the catalog's
    /// recorded lead may be stale (e.g. cross-account scan, deleted
    /// file, schema drift). See
    /// [`forge_primitives::Command::ResumeOrNewSession`] for the
    /// motivation.
    ///
    /// `cwd` is also passed to the resume attempt, not just the
    /// fresh-fallback. `claude --resume <id>` indexes sessions by the
    /// project key derived from its own working directory, so
    /// inheriting forge's `$PWD` (the default when cwd is empty) makes
    /// every cross-directory resume fail with "No conversation found
    /// with session ID …" even when the `.jsonl` exists.
    pub(crate) fn resume_or_new_session(
        &self,
        session_id: String,
        cwd: String,
        launch_settings: SessionLaunchSettings,
    ) -> anyhow::Result<()> {
        let bridge = self.clone();
        tokio::spawn(async move {
            if let Err(resume_err) =
                forge_sdk_worker::spawn_session(&bridge, &cwd, Some(&session_id), &launch_settings)
                    .await
            {
                tracing::warn!(
                    target: crate::logging::targets::BRIDGE_LIFECYCLE,
                    error = %resume_err,
                    session_id = %session_id,
                    "session resume failed; falling back to fresh session",
                );
                if let Err(new_err) =
                    forge_sdk_worker::spawn_session(&bridge, &cwd, None, &launch_settings).await
                {
                    let msg = format!(
                        "forge-sdk session spawn failed after resume fallback (resume err: {resume_err}; new err: {new_err})",
                    );
                    if bridge
                        .event_tx()
                        .send(AgentEvent::ConnectionFailed { message: msg.clone() })
                        .is_err()
                    {
                        tracing::warn!(
                            target: crate::logging::targets::BRIDGE_LIFECYCLE,
                            error = %msg,
                            "event channel closed; ConnectionFailed dropped",
                        );
                    }
                }
            }
        });
        Ok(())
    }

    // The two response handlers below are intentionally exempt from
    // `check_session_id` — staleness is detected via the pending map
    // (an unknown `tool_call_id` already logs warn in
    // `deliver_*_response`). A late response arriving after a session
    // swap should still be honoured if its tool_call_id is in the
    // shared pending map.

    pub(crate) fn permission_response(
        &self,
        _session_id: String,
        tool_call_id: String,
        outcome: PermissionOutcome,
    ) -> anyhow::Result<()> {
        forge_sdk_worker::deliver_permission_response(&self.inner.pending, &tool_call_id, outcome);
        Ok(())
    }

    pub(crate) fn question_response(
        &self,
        _session_id: String,
        tool_call_id: String,
        outcome: QuestionOutcome,
    ) -> anyhow::Result<()> {
        forge_sdk_worker::deliver_question_response(
            &self.inner.pending_questions,
            &tool_call_id,
            outcome,
        );
        Ok(())
    }

    pub(crate) fn start_git_context_watch(
        &self,
        session_id: String,
        cwd: PathBuf,
    ) -> anyhow::Result<()> {
        self.install_git_watcher(session_id, &cwd);
        Ok(())
    }

    pub(crate) fn stop_git_context_watch(&self, session_id: String) -> anyhow::Result<()> {
        self.stop_git_watcher(&session_id);
        Ok(())
    }

    // ---- Direct-return accessors (delegate to forge_sdk::*) ----

    /// Cheap clone of the bridge's bound `config_dir`. Used by the
    /// session worker to thread `CLAUDE_CONFIG_DIR` into the spawned
    /// `claude` subprocess and by direct-return accessors.
    pub(crate) fn config_dir(&self) -> PathBuf {
        self.inner.config_dir.clone()
    }

    /// Cheap clone of the bridge's bound forge-account
    /// `display_name`, when forge-workspace picked one. Used by the
    /// session worker to attach `forge_account` to the initial
    /// `StatusSnapshot` emit alongside the CLI-side `account`.
    pub(crate) fn display_name(&self) -> Option<String> {
        self.inner.display_name.clone()
    }

    pub(crate) fn project_memory_path(&self, cwd: &Path) -> PathBuf {
        crate::userdata::memory::project_memory_path(&self.inner.config_dir, cwd)
    }

    pub(crate) fn oauth_credentials(
        &self,
    ) -> Option<crate::cloud::oauth_credentials::OauthCredentials> {
        crate::cloud::oauth_credentials::load_oauth_credentials(&self.inner.config_dir)
    }

    pub(crate) fn settings_documents(
        &self,
        cwd: &Path,
    ) -> crate::userdata::settings::SettingsDocuments {
        crate::userdata::settings::settings_documents(&self.inner.config_dir, cwd)
    }

    pub(crate) fn write_settings_document(
        &self,
        target: &crate::userdata::settings::SettingsTarget,
        document: &Value,
    ) -> Result<(), forge_sdk::Error> {
        crate::userdata::settings::write_settings_document(&self.inner.config_dir, target, document)
    }

    pub(crate) async fn oauth_usage(
        &self,
    ) -> Result<crate::cloud::oauth_usage::OauthUsage, crate::cloud::oauth_usage::OauthUsageError>
    {
        crate::cloud::oauth_usage::oauth_usage(&self.inner.config_dir).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_bridge() -> ForgeSdkBridge {
        ForgeSdkBridge::new(PathBuf::from(TESTING_STUB_CONFIG_DIR), None)
    }

    #[test]
    fn take_events_returns_some_once_then_none() {
        let bridge = test_bridge();
        assert!(bridge.take_events().is_some());
        assert!(bridge.take_events().is_none());
    }

    #[test]
    fn dispatch_without_client_returns_error() {
        let bridge = test_bridge();
        let err = bridge.cancel("session-1".to_owned()).unwrap_err();
        assert!(err.to_string().contains("before active session"));
    }

    #[test]
    fn rename_session_runs_offline_without_client() {
        let bridge = test_bridge();
        // Bogus session id — `rename_session` propagates the disk
        // error rather than the "no active session" guard. The point
        // of this test is to confirm we do NOT take the dispatch path.
        let err = bridge
            .rename_session("does-not-exist-session-id".to_owned(), "title".to_owned())
            .unwrap_err();
        // Whatever forge_sdk surfaces — just ensure it isn't the
        // bridge's own "no active session" message.
        assert!(!err.to_string().contains("before active session"));
    }
}
