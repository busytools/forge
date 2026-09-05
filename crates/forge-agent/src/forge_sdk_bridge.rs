//! `ForgeSdkBridge` - in-process driver around a [`forge_sdk::Client`].
//!
//! Drives a [`forge_sdk::Client`] directly - no Node.js subprocess,
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
use tracing::Instrument;

use crate::client::{AgentEvent, SessionLaunchSettings};
use crate::forge_sdk_worker;
use forge_primitives::{PermissionMode, PermissionOutcome, QuestionOutcome};

/// Sentinel `config_dir` for `ForgeSdkBridge` test stubs that never
/// exercise the path. Production code constructs the bridge with a
/// real account `config_dir`; tests that don't drive a session use
/// this path so the typed field stays non-optional.
#[cfg(any(test, feature = "testing"))]
const TESTING_STUB_CONFIG_DIR: &str = "/tmp/forge-testing-stub";

/// Pending permission responses keyed by `tool_use_id`. The
/// `can_use_tool` callback parks a oneshot here when the CLI asks;
/// dispatch drains it when the matching `permission_response` arrives
/// from the App.
pub(crate) type PendingResponses =
    Arc<Mutex<HashMap<String, oneshot::Sender<forge_primitives::PermissionDecision>>>>;

/// Pending question outcomes keyed by `tool_use_id`. The
/// `AskUserQuestion` driver in the `can_use_tool` callback parks a
/// fresh oneshot per question, emits a `QuestionRequest`, and awaits
/// the matching `question_response`.
pub(crate) type PendingQuestions = Arc<Mutex<HashMap<String, oneshot::Sender<QuestionOutcome>>>>;

/// In-process bridge wrapping a single [`forge_sdk::Client`].
///
/// Single instance per connection. The bridge owns the spawned
/// `forge_sdk::Client`, the `can_use_tool` parking lots, and the
/// outbound `AgentEvent` channel.
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
    /// Current session id, shared with the `can_use_tool` callback so
    /// permission/question events carry the right `session_id`.
    pub(crate) session_id_slot: Arc<Mutex<String>>,
    /// `<config_dir>` this bridge was bound to at construction time
    /// (workspace-driven - typically the picked account's
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
    /// Forge-workspace-supplied in-process MCP servers attached at
    /// every `spawn_session` call. Today this is the per-session
    /// `forge` MCP server with the four peer-coordination tools;
    /// future modules (worktree, memory, …) slot in alongside under
    /// the same `forge` server name. Cheap to clone - each entry
    /// is just a name + a few `Arc<dyn Tool>`s.
    extra_mcp_servers: Vec<(String, forge_sdk::mcp::McpServer)>,
    /// The session's resolved forge.toml env - `[env]` merged with
    /// `[accounts.env]` and the spawning project's
    /// `[projects.<name>.env]` - stamped onto the spawned `claude`
    /// subprocess by `forge_sdk_worker::build_options_with_callback`.
    /// Empty when `Agent::spawn` is called directly without a
    /// workspace (tests, smoke) or when no table declares anything.
    env: HashMap<String, String>,
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
    pub(crate) fn new(
        config_dir: PathBuf,
        display_name: Option<String>,
        extra_mcp_servers: Vec<(String, forge_sdk::mcp::McpServer)>,
        env: HashMap<String, String>,
    ) -> Self {
        let (event_tx, events_rx) = mpsc::unbounded_channel();
        Self {
            inner: Arc::new(BridgeInner {
                client: Mutex::new(None),
                event_tx,
                events_rx: Mutex::new(Some(events_rx)),
                pending: Arc::new(Mutex::new(HashMap::new())),
                pending_questions: Arc::new(Mutex::new(HashMap::new())),
                session_id_slot: Arc::new(Mutex::new(String::new())),
                config_dir,
                display_name,
                extra_mcp_servers,
                env,
            }),
        }
    }

    /// The resolved forge.toml env to stamp onto every spawned
    /// `claude` subprocess. Cloned per `spawn_session` call, but the
    /// values were read from disk once at forge BOOT - a new bridge
    /// does not re-read them, so a forge.toml edit needs a forge
    /// restart, not a new session.
    pub(crate) fn env(&self) -> HashMap<String, String> {
        self.inner.env.clone()
    }

    /// Forge-workspace-supplied in-process MCP servers to attach to
    /// every spawned `claude` subprocess (e.g. the `forge` server
    /// carrying the peer-coordination tools - #114 v1). Cheap-clone
    /// via the `McpServer`'s `Arc<dyn Tool>` internals; called once
    /// per `spawn_session` invocation.
    pub(crate) fn extra_mcp_servers(&self) -> Vec<(String, forge_sdk::mcp::McpServer)> {
        self.inner.extra_mcp_servers.clone()
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
        // Drain in-flight permission / question oneshots before the
        // client goes away. The SDK's detached `can_use_tool` /
        // `question` callback tasks are awaiting these receivers; if
        // we drop the client without resolving them they leak the
        // closure (which holds `Arc<event_tx>`, `Arc<PendingResponses>`,
        // …) and the matching workspace forwarders exit on a closed
        // channel without notifying the awaiter.
        for (_, tx) in self.inner.pending.lock().drain() {
            let _ = tx.send(forge_primitives::PermissionDecision::deny("session replaced"));
        }
        for (_, tx) in self.inner.pending_questions.lock().drain() {
            let _ = tx.send(forge_primitives::QuestionOutcome::Cancelled);
        }
        self.inner.client.lock().take()
    }

    /// Send an `AgentEvent::McpOperationError` to the App; log a
    /// `BRIDGE_LIFECYCLE` warn if the channel is closed (terminal
    /// event for a user-visible MCP failure - silent-drop on
    /// teardown race would lose the only path to surface).
    /// On send failure we destructure the unsent event back out of
    /// the `SendError` so the warn log carries `session_id`,
    /// `server_name`, and the underlying error text - the most
    /// actionable triage signals.
    fn emit_mcp_error_or_log(
        event_tx: &mpsc::UnboundedSender<AgentEvent>,
        session_id: String,
        operation: &'static str,
        server_name: Option<String>,
        error_msg: String,
    ) {
        let session_id_for_log = session_id.clone();
        let server_name_for_log = server_name.clone();
        let error_msg_for_log = error_msg.clone();
        let event = AgentEvent::McpOperationError {
            session_id,
            error: forge_primitives::McpOperationError {
                operation: operation.to_owned(),
                server_name,
                message: error_msg,
            },
        };
        if event_tx.send(event).is_err() {
            tracing::warn!(
                target: crate::logging::targets::BRIDGE_LIFECYCLE,
                session_id = %session_id_for_log,
                operation,
                server_name = ?server_name_for_log,
                error_msg = %error_msg_for_log,
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
        self.dispatch_with_failure(label, f, |_| None)
    }

    /// Spawn a fire-and-forget client call. A failure inside the
    /// spawned future - or the client-less early Err - is emitted to
    /// the App as the caller's typed event (a `None` maps to
    /// log-only) - the TUI rolled back nothing otherwise: the
    /// spinner, the optimistic chip, the waiting awaiter all unwind
    /// only if this event arrives.
    fn dispatch_with_failure<F, Fut, T>(
        &self,
        label: &'static str,
        f: F,
        on_failure: T,
    ) -> anyhow::Result<()>
    where
        F: FnOnce(Client) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
        T: FnOnce(&anyhow::Error) -> Option<AgentEvent> + Send + 'static,
    {
        let Some(client) = self.client() else {
            let err = anyhow::anyhow!("forge-sdk bridge: {label} called before active session");
            // The caller warn-logs this Err and drops it; the typed
            // event is what unwinds the TUI's optimistic state.
            if let Some(event) = on_failure(&err)
                && self.inner.event_tx.send(event).is_err()
            {
                tracing::warn!(
                    target: crate::logging::targets::BRIDGE_LIFECYCLE,
                    label,
                    "event channel closed; typed dispatch failure dropped",
                );
            }
            return Err(err);
        };
        let event_tx = self.inner.event_tx.clone();
        let span = tracing::info_span!("bridge_dispatch", label);
        tokio::spawn(
            async move {
                if let Err(err) = f(client).await {
                    tracing::warn!(
                        target: crate::logging::targets::BRIDGE_LIFECYCLE,
                        label,
                        error = %err,
                        "forge-sdk bridge: dispatch failed",
                    );
                    if let Some(event) = on_failure(&err)
                        && event_tx.send(event).is_err()
                    {
                        tracing::warn!(
                            target: crate::logging::targets::BRIDGE_LIFECYCLE,
                            label,
                            "event channel closed; typed dispatch failure dropped",
                        );
                    }
                }
            }
            .instrument(span),
        );
        Ok(())
    }
}

#[cfg(any(test, feature = "testing"))]
impl Default for ForgeSdkBridge {
    fn default() -> Self {
        Self::new(PathBuf::from(TESTING_STUB_CONFIG_DIR), None, Vec::new(), HashMap::new())
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
        // No `check_session_id` here - the TUI commits to
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
        self.dispatch_with_failure(
            "prompt",
            move |client| async move { forge_sdk_worker::send_prompt(&client, chunks).await },
            move |err| {
                Some(AgentEvent::TurnError {
                    session_id: session_id.clone(),
                    message: err.to_string(),
                })
            },
        )
    }

    pub(crate) fn cancel(&self, session_id: String) -> anyhow::Result<()> {
        if !self.check_session_id(&session_id, "cancel") {
            return Ok(());
        }
        let event_tx = self.inner.event_tx.clone();
        let err_session_id = session_id.clone();
        self.dispatch_with_failure(
            "cancel",
            move |client| async move {
                match tokio::time::timeout(Self::CONTROL_RESPONSE_TIMEOUT, client.interrupt()).await
                {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(e.into()),
                    Err(_) => {
                        let message = "interrupt not acknowledged by the CLI".to_owned();
                        if event_tx
                            .send(AgentEvent::TurnError {
                                session_id: session_id.clone(),
                                message: message.clone(),
                            })
                            .is_err()
                        {
                            tracing::warn!(
                                target: crate::logging::targets::BRIDGE_LIFECYCLE,
                                "event channel closed; TurnError dropped",
                            );
                        }
                        Ok(())
                    }
                }
            },
            move |err| {
                Some(AgentEvent::TurnError {
                    session_id: err_session_id,
                    message: format!("interrupt not acknowledged: {err}"),
                })
            },
        )
    }

    /// `send_control` parks until the CLI's `control_response`; the
    /// wait is bounded so a silent CLI surfaces as a typed failure
    /// instead of a chip/spinner stuck on an optimistic change that
    /// never took.
    const CONTROL_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

    /// The CLI's own refusal text for a failed `set_permission_mode`.
    /// The SDK wraps it twice (`MessageParse` Display + a
    /// `control failed: ` prefix); strip both so the chat line carries
    /// the reason, not the wrapper.
    fn set_mode_rejection_text(err: &forge_sdk::Error) -> String {
        if let forge_sdk::Error::MessageParse { reason, .. } = err
            && let Some(cli_reason) = reason.strip_prefix("control failed: ")
        {
            return cli_reason.to_owned();
        }
        err.to_string()
    }

    pub(crate) fn set_mode(&self, session_id: String, mode: PermissionMode) -> anyhow::Result<()> {
        if !self.check_session_id(&session_id, "set_mode") {
            // A session-swap race dropped the dispatch; surface it on
            // the attempted session so the chip doesn't stay flipped.
            let _ = self.inner.event_tx.send(AgentEvent::SetModeFailed {
                session_id,
                mode,
                message: "the session it was sent to is no longer active".to_owned(),
            });
            return Ok(());
        }
        let event_tx = self.inner.event_tx.clone();
        let err_session_id = session_id.clone();
        self.dispatch_with_failure(
            "set_mode",
            move |client| async move {
                let outcome = tokio::time::timeout(
                    Self::CONTROL_RESPONSE_TIMEOUT,
                    client.set_permission_mode(mode),
                )
                .await;
                let failure = match outcome {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => {
                        let text = Self::set_mode_rejection_text(&e);
                        Some(if text.trim().is_empty() {
                            "no reason given".to_owned()
                        } else {
                            text
                        })
                    }
                    Err(_) => Some("no response from the CLI".to_owned()),
                };
                if let Some(message) = failure {
                    if event_tx
                        .send(AgentEvent::SetModeFailed {
                            session_id: session_id.clone(),
                            mode,
                            message: message.clone(),
                        })
                        .is_err()
                    {
                        tracing::warn!(
                            target: crate::logging::targets::BRIDGE_LIFECYCLE,
                            error = %message,
                            "event channel closed; SetModeFailed dropped",
                        );
                    } else {
                        tracing::warn!(
                            target: crate::logging::targets::BRIDGE_LIFECYCLE,
                            session_id = %session_id,
                            mode = %mode.as_wire(),
                            error = %message,
                            "set_permission_mode rejected; SetModeFailed emitted",
                        );
                    }
                }
                Ok(())
            },
            move |err| {
                Some(AgentEvent::SetModeFailed {
                    session_id: err_session_id,
                    mode,
                    message: format!("set_mode never reached the CLI: {err}"),
                })
            },
        )
    }

    pub(crate) fn set_model(&self, session_id: String, model: String) -> anyhow::Result<()> {
        if !self.check_session_id(&session_id, "set_model") {
            // A session-swap race dropped the dispatch; surface it on
            // the attempted session so the model chip doesn't stay
            // flipped.
            let _ = self.inner.event_tx.send(AgentEvent::SetModelFailed {
                session_id: session_id.clone(),
                model: model.clone(),
                message: "the session it was sent to is no longer active".to_owned(),
            });
            return Ok(());
        }
        let event_tx = self.inner.event_tx.clone();
        let err_session_id = session_id.clone();
        let err_model = model.clone();
        self.dispatch_with_failure(
            "set_model",
            move |client| async move {
                let outcome = tokio::time::timeout(
                    Self::CONTROL_RESPONSE_TIMEOUT,
                    client.set_model(Some(model.as_str())),
                )
                .await;
                let failure = match outcome {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => {
                        let text = Self::set_mode_rejection_text(&e);
                        Some(if text.trim().is_empty() {
                            "no reason given".to_owned()
                        } else {
                            text
                        })
                    }
                    Err(_) => Some("no response from the CLI".to_owned()),
                };
                if let Some(message) = failure
                    && event_tx
                        .send(AgentEvent::SetModelFailed {
                            session_id: session_id.clone(),
                            model: model.clone(),
                            message: message.clone(),
                        })
                        .is_err()
                {
                    tracing::warn!(
                        target: crate::logging::targets::BRIDGE_LIFECYCLE,
                        error = %message,
                        "event channel closed; SetModelFailed dropped",
                    );
                }
                Ok(())
            },
            move |err| {
                Some(AgentEvent::SetModelFailed {
                    session_id: err_session_id,
                    model: err_model,
                    message: format!("set_model never reached the CLI: {err}"),
                })
            },
        )
    }

    /// Verify the requested `session_id` matches the bridge's current
    /// session before dispatching a user-action method. In a session-swap
    /// race a `cancel`/`set_mode`/`set_model` for session A could
    /// otherwise hit session B's `Client`. Emits a debug breadcrumb
    /// here on mismatch and returns false; the caller is expected to
    /// drop the dispatch with a no-op `Ok(())`.
    ///
    /// Other user-action methods (`prompt_with_images`) intentionally
    /// opt out - see the inline rationale at each call site.
    fn check_session_id(&self, session_id: &str, label: &'static str) -> bool {
        let current = self.inner.session_id_slot.lock().clone();
        if current.is_empty() || current == session_id {
            return true;
        }
        tracing::warn!(
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

    pub(crate) fn get_status_snapshot(&self, session_id: String) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        let config_dir = self.inner.config_dir.clone();
        let display_name = self.inner.display_name.clone();
        let env = self.inner.env.clone();
        self.dispatch("get_status_snapshot", move |client| async move {
            // The shell fallback shells out to `claude auth status`;
            // wrap in spawn_blocking so this dispatched task doesn't
            // park its tokio worker for the ~50ms probe.
            let account = if let Some(account) = client.account_info_from_init() {
                account
            } else {
                let cd = config_dir.clone();
                match tokio::task::spawn_blocking(move || {
                    crate::cloud::auth_status::shell_identity_fallback(&cd, &env)
                })
                .await
                {
                    Ok(opt) => opt.unwrap_or_default(),
                    Err(join_err) => {
                        tracing::warn!(
                            target: crate::logging::targets::BRIDGE_LIFECYCLE,
                            error = %join_err,
                            "get_status_snapshot account probe spawn_blocking task panicked"
                        );
                        forge_primitives::AccountInfo::default()
                    }
                }
            };
            let forge_account = display_name.map(forge_primitives::ForgeAccountIdentity::new);
            if event_tx
                .send(AgentEvent::StatusSnapshot { session_id, account, forge_account })
                .is_err()
            {
                tracing::warn!(
                    target: crate::logging::targets::BRIDGE_LIFECYCLE,
                    "event channel closed; StatusSnapshot dropped",
                );
            }
            Ok(())
        })
    }

    pub(crate) fn get_oauth_credentials_snapshot(&self, session_id: String) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        let config_dir = self.inner.config_dir.clone();
        let env = self.inner.env.clone();
        self.dispatch("get_oauth_credentials_snapshot", move |_client| async move {
            // load_oauth_credentials shells out to macOS `security
            // find-generic-password`; wrap in spawn_blocking so the
            // 30s usage-poller doesn't park N tokio workers per
            // account during keychain access.
            let credentials = match tokio::task::spawn_blocking(move || {
                crate::cloud::oauth_credentials::session_oauth_credentials(&config_dir, &env)
            })
            .await
            {
                Ok(opt) => opt,
                Err(join_err) => {
                    tracing::warn!(
                        target: crate::logging::targets::BRIDGE_LIFECYCLE,
                        error = %join_err,
                        "load_oauth_credentials spawn_blocking task panicked"
                    );
                    None
                }
            };
            if event_tx
                .send(AgentEvent::OauthCredentialsSnapshot { session_id, credentials })
                .is_err()
            {
                tracing::warn!(
                    target: crate::logging::targets::BRIDGE_LIFECYCLE,
                    "event channel closed; OauthCredentialsSnapshot dropped",
                );
            }
            Ok(())
        })
    }

    pub(crate) fn get_context_usage(&self, session_id: String) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        self.dispatch("get_context_usage", move |client| async move {
            // The footer poll fires this every few seconds; a wedged
            // CLI must cost one skipped poll, not a parked task per
            // poll with the Ctx bar frozen. Accept-and-document: the
            // expiry stays log-only (no typed event) because the bar
            // showing a stale percentage is preferable to flashing an
            // error - the next poll re-fires and recovers on its own.
            let usage = match tokio::time::timeout(
                Self::CONTROL_RESPONSE_TIMEOUT,
                client.get_context_usage(),
            )
            .await
            {
                Ok(Ok(usage)) => usage,
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => {
                    tracing::warn!(
                        target: crate::logging::targets::BRIDGE_LIFECYCLE,
                        "context usage probe timed out; skipping this poll",
                    );
                    return Ok(());
                }
            };
            let percentage = forge_sdk_worker::clamp_percentage_to_u8(usage.percentage);
            // `raw_max_tokens` is the model's nominal context-window
            // size; `max_tokens` is the effective cap after autocompact
            // reductions. Forge surfaces the raw size so the panel
            // shows the model's headline capacity (1M / 200K / …)
            // rather than a fluctuating effective number.
            let max_tokens = Some(usage.raw_max_tokens);
            if event_tx
                .send(AgentEvent::ContextUsage {
                    session_id,
                    percentage: Some(percentage),
                    max_tokens,
                })
                .is_err()
            {
                tracing::warn!(
                    target: crate::logging::targets::BRIDGE_LIFECYCLE,
                    "event channel closed; ContextUsage dropped",
                );
            }
            Ok(())
        })
    }

    pub(crate) fn reload_plugins(&self, session_id: String) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        let err_session_id = session_id.clone();
        self.dispatch_with_failure(
            "reload_plugins",
            move |client| async move {
                let outcome =
                    tokio::time::timeout(Self::CONTROL_RESPONSE_TIMEOUT, client.reload_plugins())
                        .await;
                match outcome {
                    Ok(Ok(_)) => {
                        if event_tx.send(AgentEvent::RuntimeReloadCompleted { session_id }).is_err()
                        {
                            tracing::warn!(
                                target: crate::logging::targets::BRIDGE_LIFECYCLE,
                                "event channel closed; RuntimeReloadCompleted dropped",
                            );
                        }
                    }
                    Ok(Err(e)) => {
                        let msg = format!("reload_plugins failed: {e}");
                        if event_tx
                            .send(AgentEvent::RuntimeReloadFailed {
                                session_id,
                                message: msg.clone(),
                            })
                            .is_err()
                        {
                            tracing::warn!(
                                target: crate::logging::targets::BRIDGE_LIFECYCLE,
                                error = %msg,
                                "event channel closed; RuntimeReloadFailed dropped",
                            );
                        }
                    }
                    Err(_) => {
                        let msg = "no response from the CLI".to_owned();
                        if event_tx
                            .send(AgentEvent::RuntimeReloadFailed {
                                session_id,
                                message: msg.clone(),
                            })
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
            },
            move |err| {
                Some(AgentEvent::RuntimeReloadFailed {
                    session_id: err_session_id,
                    message: format!("reload_plugins never reached the CLI: {err}"),
                })
            },
        )
    }

    pub(crate) fn get_mcp_snapshot(&self, session_id: String) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        let err_session_id = session_id.clone();
        self.dispatch_with_failure(
            "get_mcp_snapshot",
            move |client| async move {
                let outcome =
                    tokio::time::timeout(Self::CONTROL_RESPONSE_TIMEOUT, client.mcp_status()).await;
                let response = match outcome {
                    Ok(Err(e)) => return Err(e.into()),
                    Ok(Ok(result)) => result,
                    Err(_) => {
                        Self::emit_mcp_error_or_log(
                            &event_tx,
                            session_id,
                            "status",
                            None,
                            "no response from the CLI".to_owned(),
                        );
                        return Ok(());
                    }
                };
                if event_tx
                    .send(AgentEvent::McpSnapshot {
                        session_id,
                        servers: response.mcp_servers,
                        error: None,
                    })
                    .is_err()
                {
                    tracing::warn!(
                        target: crate::logging::targets::BRIDGE_LIFECYCLE,
                        "event channel closed; McpSnapshot dropped",
                    );
                }
                Ok(())
            },
            move |err| {
                Some(AgentEvent::McpOperationError {
                    session_id: err_session_id,
                    error: forge_primitives::McpOperationError {
                        operation: "status".to_owned(),
                        server_name: None,
                        message: err.to_string(),
                    },
                })
            },
        )
    }

    pub(crate) fn reconnect_mcp_server(
        &self,
        session_id: String,
        server_name: String,
    ) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        let err_session_id = session_id.clone();
        let err_server_name = server_name.clone();
        self.dispatch_with_failure(
            "reconnect_mcp_server",
            move |client| async move {
                match tokio::time::timeout(
                    Self::CONTROL_RESPONSE_TIMEOUT,
                    client.mcp_reconnect(&server_name),
                )
                .await
                {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(e.into()),
                    Err(_) => {
                        Self::emit_mcp_error_or_log(
                            &event_tx,
                            session_id,
                            "reconnect",
                            Some(server_name),
                            "no response from the CLI".to_owned(),
                        );
                        Ok(())
                    }
                }
            },
            move |err| {
                Some(AgentEvent::McpOperationError {
                    session_id: err_session_id,
                    error: forge_primitives::McpOperationError {
                        operation: "reconnect".to_owned(),
                        server_name: Some(err_server_name),
                        message: err.to_string(),
                    },
                })
            },
        )
    }

    pub(crate) fn toggle_mcp_server(
        &self,
        session_id: String,
        server_name: String,
        enabled: bool,
    ) -> anyhow::Result<()> {
        let event_tx = self.inner.event_tx.clone();
        let err_session_id = session_id.clone();
        let err_server_name = server_name.clone();
        self.dispatch_with_failure(
            "toggle_mcp_server",
            move |client| async move {
                match tokio::time::timeout(
                    Self::CONTROL_RESPONSE_TIMEOUT,
                    client.mcp_toggle(&server_name, enabled),
                )
                .await
                {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(e.into()),
                    Err(_) => {
                        Self::emit_mcp_error_or_log(
                            &event_tx,
                            session_id,
                            "toggle",
                            Some(server_name),
                            "no response from the CLI".to_owned(),
                        );
                        Ok(())
                    }
                }
            },
            move |err| {
                Some(AgentEvent::McpOperationError {
                    session_id: err_session_id,
                    error: forge_primitives::McpOperationError {
                        operation: "toggle".to_owned(),
                        server_name: Some(err_server_name),
                        message: err.to_string(),
                    },
                })
            },
        )
    }

    pub(crate) fn new_session(
        &self,
        cwd: String,
        launch_settings: SessionLaunchSettings,
    ) -> anyhow::Result<()> {
        let bridge = self.clone();
        let span = tracing::info_span!("bridge_new_session", cwd = %cwd);
        tokio::spawn(
            async move {
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
            }
            .instrument(span),
        );
        Ok(())
    }

    pub(crate) fn resume_session(
        &self,
        session_id: String,
        cwd: String,
        launch_settings: SessionLaunchSettings,
    ) -> anyhow::Result<()> {
        let bridge = self.clone();
        let span = tracing::info_span!(
            "bridge_resume_session",
            session_id = %session_id,
            cwd = %cwd,
        );
        tokio::spawn(
            async move {
                if let Err(err) = forge_sdk_worker::spawn_session(
                    &bridge,
                    &cwd,
                    Some(&session_id),
                    &launch_settings,
                )
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
            }
            .instrument(span),
        );
        Ok(())
    }

    /// Try resume; on failure transparently retry as new_session(cwd).
    /// Surfaces `ConnectionFailed` only when both attempts fail. Used
    /// by project-rooted spawns (Default / Named) where the catalog's
    /// recorded lead may be stale (e.g. cross-account scan, deleted
    /// file, schema drift). See
    /// [`forge_primitives::AgentCommand::ResumeOrNewSession`] for the
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
        let span = tracing::info_span!(
            "bridge_resume_or_new_session",
            session_id = %session_id,
            cwd = %cwd,
        );
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
        }.instrument(span));
        Ok(())
    }

    // The two response handlers below are intentionally exempt from
    // `check_session_id` - staleness is detected via the pending map
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

    /// OS PID of the spawned `claude` child, when a client is bound
    /// and the transport reported one. Used by
    /// [`crate::env::processes`] to anchor an OS-level walk of the
    /// descendant process tree for the Inspector pane's PROCESSES
    /// section. Returns `None` before the first `new_session` /
    /// `resume_session` lands a client, or after `clear_client`.
    pub(crate) fn claude_pid(&self) -> Option<u32> {
        self.client().and_then(|c| c.claude_pid())
    }

    pub(crate) fn project_memory_path(&self, cwd: &Path) -> PathBuf {
        crate::userdata::memory::project_memory_path(&self.inner.config_dir, cwd)
    }

    pub(crate) fn settings_documents(
        &self,
        cwd: &Path,
    ) -> crate::userdata::settings::SettingsDocuments {
        crate::userdata::settings::settings_documents(&self.inner.config_dir, cwd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_bridge() -> ForgeSdkBridge {
        ForgeSdkBridge::new(
            PathBuf::from(TESTING_STUB_CONFIG_DIR),
            None,
            Vec::new(),
            HashMap::new(),
        )
    }

    #[test]
    fn take_events_returns_some_once_then_none() {
        let bridge = test_bridge();
        assert!(bridge.take_events().is_some());
        assert!(bridge.take_events().is_none());
    }

    #[test]
    fn clear_client_drains_pending_permission_map() {
        let bridge = test_bridge();
        let (tx, rx) = tokio::sync::oneshot::channel::<forge_primitives::PermissionDecision>();
        bridge.inner.pending.lock().insert("tool-id-1".to_owned(), tx);

        bridge.clear_client();

        assert!(bridge.inner.pending.lock().is_empty());
        let decision = rx.blocking_recv().expect("oneshot resolved by clear_client drain");
        assert!(!decision.is_allow(), "drain must resolve with deny");
    }

    #[test]
    fn clear_client_drains_pending_question_map() {
        let bridge = test_bridge();
        let (tx, rx) = tokio::sync::oneshot::channel::<forge_primitives::QuestionOutcome>();
        bridge.inner.pending_questions.lock().insert("tool-id-q1".to_owned(), tx);

        bridge.clear_client();

        assert!(bridge.inner.pending_questions.lock().is_empty());
        let outcome = rx.blocking_recv().expect("oneshot resolved by clear_client drain");
        assert!(matches!(outcome, forge_primitives::QuestionOutcome::Cancelled));
    }

    #[test]
    fn dispatch_without_client_returns_error() {
        let bridge = test_bridge();
        let err = bridge.cancel("session-1".to_owned()).unwrap_err();
        assert!(err.to_string().contains("before active session"));
    }

    /// The client-None window (clear_client, then set_client only
    /// after init completes) still routes commands with the old
    /// session id, so the caller's typed event is the only thing that
    /// unwinds the TUI's optimistic state - a silent Err return loses
    /// the prompt entirely.
    #[tokio::test]
    async fn dispatch_without_client_emits_the_typed_event() {
        let bridge = test_bridge();
        let mut events = bridge.take_events().expect("fresh bridge yields its events receiver");

        let err = bridge
            .dispatch_with_failure(
                "prompt",
                |_client| async { Ok::<(), anyhow::Error>(()) },
                |err| {
                    Some(AgentEvent::TurnError {
                        session_id: "s1".to_owned(),
                        message: err.to_string(),
                    })
                },
            )
            .expect_err("no client, dispatch refused");
        assert!(err.to_string().contains("before active session"));

        match tokio::time::timeout(std::time::Duration::from_secs(2), events.recv()).await {
            Ok(Some(AgentEvent::TurnError { message, .. })) => {
                assert!(
                    message.contains("before active session"),
                    "the typed failure carries the early Err: {message}"
                );
            }
            other => panic!("expected a TurnError from the client-less early Err, got {other:?}"),
        }
    }

    /// The client-None window passes `check_session_id` for both
    /// sibling methods (the slot holds the old session id, or is
    /// empty), so the typed failure is the only thing that unflips the
    /// optimistic mode chip - a silent Err return leaves it stuck.
    #[tokio::test]
    async fn set_mode_without_client_emits_set_mode_failed() {
        let bridge = test_bridge();
        let mut events = bridge.take_events().expect("fresh bridge yields its events receiver");

        bridge
            .set_mode("session-1".to_owned(), PermissionMode::AcceptEdits)
            .expect_err("no client, dispatch refused");

        match tokio::time::timeout(std::time::Duration::from_secs(2), events.recv()).await {
            Ok(Some(AgentEvent::SetModeFailed { session_id, mode, message })) => {
                assert_eq!(session_id, "session-1");
                assert_eq!(mode, PermissionMode::AcceptEdits);
                assert!(
                    message.contains("set_mode never reached the CLI"),
                    "the typed failure names the op and carries the early Err: {message}"
                );
            }
            other => panic!("expected SetModeFailed from the client-less early Err, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_model_without_client_emits_set_model_failed() {
        let bridge = test_bridge();
        let mut events = bridge.take_events().expect("fresh bridge yields its events receiver");

        bridge
            .set_model("session-1".to_owned(), "claude-sonnet-5".to_owned())
            .expect_err("no client, dispatch refused");

        match tokio::time::timeout(std::time::Duration::from_secs(2), events.recv()).await {
            Ok(Some(AgentEvent::SetModelFailed { session_id, model, message })) => {
                assert_eq!(session_id, "session-1");
                assert_eq!(model, "claude-sonnet-5");
                assert!(
                    message.contains("set_model never reached the CLI"),
                    "the typed failure names the op and carries the early Err: {message}"
                );
            }
            other => {
                panic!("expected SetModelFailed from the client-less early Err, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn reload_plugins_without_client_emits_runtime_reload_failed() {
        let bridge = test_bridge();
        let mut events = bridge.take_events().expect("fresh bridge yields its events receiver");

        bridge.reload_plugins("session-1".to_owned()).expect_err("no client, dispatch refused");

        match tokio::time::timeout(std::time::Duration::from_secs(2), events.recv()).await {
            Ok(Some(AgentEvent::RuntimeReloadFailed { session_id, message })) => {
                assert_eq!(session_id, "session-1");
                assert!(
                    message.contains("reload_plugins never reached the CLI"),
                    "the typed failure names the op and carries the early Err: {message}"
                );
            }
            other => {
                panic!("expected RuntimeReloadFailed from the client-less early Err, got {other:?}")
            }
        }
    }

    #[test]
    fn set_mode_rejection_text_strips_the_control_failed_wrapper() {
        let err = forge_sdk::Error::message_parse("control failed: mode not permitted");
        assert_eq!(ForgeSdkBridge::set_mode_rejection_text(&err), "mode not permitted");
        let other = forge_sdk::Error::Connection {
            reason: "subprocess closed before set_permission_mode response".into(),
        };
        assert_eq!(
            ForgeSdkBridge::set_mode_rejection_text(&other),
            other.to_string(),
            "non-control errors pass through as their Display",
        );
    }

    /// forge-sdk's shared dev fixture (same file the forge-sdk
    /// integration tests drive); the bridge only needs a completed
    /// handshake behind `set_client`.
    fn sdk_mock_binary() -> String {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../forge-sdk/tests/fixtures/mock_claude.sh")
            .to_owned()
    }

    async fn bridge_with_mock_client() -> (ForgeSdkBridge, mpsc::UnboundedReceiver<AgentEvent>) {
        bridge_with_mock_client_opts(forge_sdk::OptionsBuilder::new()).await
    }

    async fn bridge_with_mock_client_opts(
        builder: forge_sdk::OptionsBuilder,
    ) -> (ForgeSdkBridge, mpsc::UnboundedReceiver<AgentEvent>) {
        let bridge = ForgeSdkBridge::default();
        let events = bridge.take_events().expect("fresh bridge yields its events receiver");
        let opts = builder.binary(sdk_mock_binary()).build();
        let (client, _client_events) = forge_sdk::Client::spawn(opts).await.expect("mock client");
        bridge.set_client(client);
        (bridge, events)
    }

    /// A failure inside the spawned dispatch future reaches the App as
    /// the caller's typed event - the spinner/optimistic state unwinds
    /// only when this arrives, so log-only failure is a regression.
    #[tokio::test]
    async fn dispatch_failure_emits_the_typed_event() {
        let (bridge, mut events) = bridge_with_mock_client().await;

        bridge
            .dispatch_with_failure(
                "prompt",
                |_client| async { Err::<(), _>(anyhow::anyhow!("stdin write failed")) },
                |err| {
                    Some(AgentEvent::TurnError {
                        session_id: "s1".to_owned(),
                        message: err.to_string(),
                    })
                },
            )
            .expect("dispatch accepted");

        match tokio::time::timeout(std::time::Duration::from_secs(2), events.recv()).await {
            Ok(Some(AgentEvent::TurnError { message, .. })) => {
                assert!(
                    message.contains("stdin write failed"),
                    "the typed failure carries the error: {message}"
                );
            }
            other => panic!("expected a TurnError from the dispatch Err arm, got {other:?}"),
        }
    }

    /// A CLI that never answers `set_model` must surface
    /// `SetModelFailed` at the 60s budget (virtual time) instead of
    /// leaving the optimistic model flip unrecoverable. The wedge rides
    /// `Options::env` so the mock subprocess alone skips the subtype.
    /// Time runs REAL for the spawn handshake (a paused clock would
    /// fire the init budget before the mock's real stdout arrives),
    /// then pauses so the 60s dispatch budget elapses virtually.
    #[tokio::test(start_paused = true)]
    async fn set_model_timeout_emits_set_model_failed() {
        tokio::time::resume();
        let (bridge, mut events) = bridge_with_mock_client_opts(
            forge_sdk::OptionsBuilder::new().env("FORGED_MOCK_SKIP_SUBTYPE", "set_model"),
        )
        .await;
        tokio::time::pause();

        bridge
            .set_model("mock-session-001".to_owned(), "claude-attempted".to_owned())
            .expect("dispatch");

        // Outer wrapper past the 60s budget: virtual time advances to
        // the SOONEST timer, so the dispatch's 60s fires before this.
        // Either failure text is a legitimate SetModelFailed cause: the
        // timeout's "no response from the CLI", or the connection
        // failure's Display when the mock's stdout ends inside the
        // virtual-time window. The test's job is the mechanism, not
        // which wedge won.
        match tokio::time::timeout(std::time::Duration::from_secs(120), events.recv()).await {
            Ok(Some(AgentEvent::SetModelFailed { model, message, .. })) => {
                assert_eq!(model, "claude-attempted");
                assert!(
                    message == "no response from the CLI"
                        || message.contains("connection to claude subprocess failed"),
                    "a SetModelFailed failure text is required: {message}"
                );
            }
            other => panic!("expected SetModelFailed on the wedged set_model, got {other:?}"),
        }
    }
}
