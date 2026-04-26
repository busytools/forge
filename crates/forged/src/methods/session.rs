//! `session.*` method handlers.
//!
//! Each handler is a thin proxy over the session's actor task — they
//! enqueue a [`Command`] on the session's mpsc and `.await` the actor's
//! reply. The actor (spawned at `session.spawn`) is the sole owner of
//! the [`forge_sdk::Client`]; locking the [`Client`] from multiple
//! tasks would deadlock because [`forge_sdk::Client::next_event`]
//! holds `&mut self` across subprocess I/O.

use std::sync::Arc;

use forge_sdk::agents::{EffortLevel, EffortPreset};
use forge_sdk::{
    Client, Options, OptionsBuilder, PermissionMode, SdkPluginConfig, SystemPromptKind,
    ThinkingConfig,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;
use tracing::info;
use uuid::Uuid;

use crate::Error;
use crate::registry::DaemonState;
use crate::sdk_callbacks::WireHookSpec;
use crate::session_state::{Command, SessionHandle, SessionId};

/// Result of `session.spawn`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SpawnResult {
    /// Daemon-minted id for the freshly-spawned session.
    pub session_id: SessionId,
}

/// `session.spawn` — create a new claude session inside the daemon and
/// boot its actor task.
///
/// Uses [`crate::bridged_transport::BridgedTransport`] rather than
/// [`forge_sdk::Client::spawn`] so the actor can [`tokio::select!`]
/// between [`forge_sdk::Client::next_event`] reads and command-driven
/// writes without holding the [`Client`] lock across blocking I/O. See
/// the bridged-transport module docs for the full rationale.
///
/// Wires in M4's reverse-RPC bridges before spawning:
///   - [`ForgedPermissionBridge`](crate::sdk_callbacks::ForgedPermissionBridge)
///     as `options.can_use_tool` — routes `permission.request` over
///     reverse-RPC to the session's primary client.
///   - One [`ForgedHookBridge`](crate::sdk_callbacks::ForgedHookBridge)
///     per [`WireHookSpec`] in `params.hooks` — routes `hook.<kind>`
///     over reverse-RPC.
///
/// # Errors
///
/// Bubbles `forge_sdk::Error` for spawn failures.
pub async fn spawn(
    state: &DaemonState,
    params: impl Into<SpawnParams>,
) -> Result<SpawnResult, Error> {
    let SpawnParams {
        mut options,
        hooks: hook_specs,
    } = params.into();

    let session_id = SessionId(format!("sess_{}", Uuid::new_v4()));
    let state_arc = Arc::new(state.clone());

    // Attach the reverse-RPC permission bridge so `can_use_tool` over
    // the wire goes through this session's primary client.
    let perm_bridge =
        crate::sdk_callbacks::ForgedPermissionBridge::new(state_arc.clone(), session_id.clone());
    options.can_use_tool = Some(Arc::new(perm_bridge));

    // Attach hook bridges per spec. Replaces any default-empty Hooks
    // the wire deserialiser left behind.
    if !hook_specs.is_empty() {
        let hooks = crate::sdk_callbacks::attach_hooks(&state_arc, &session_id, &hook_specs)?;
        options.hooks = hooks;
    }

    let bridge = crate::bridged_transport::BridgedTransport::spawn(&options)
        .await
        .map_err(Error::Sdk)?;
    let client = Client::spawn_with_transport(options, Box::new(bridge))
        .await
        .map_err(Error::Sdk)?;
    let (handle, rx) = state.register_session(session_id.clone());
    spawn_session_actor(state.clone(), &handle, client, rx);
    info!(session_id = %session_id.0, "session spawned");
    Ok(SpawnResult { session_id })
}

/// Parsed `session.spawn` params: the configured [`Options`] plus the
/// hook-spec list (M4) used to attach
/// [`ForgedHookBridge`](crate::sdk_callbacks::ForgedHookBridge) instances
/// after the session id is minted.
#[derive(Debug)]
#[non_exhaustive]
pub struct SpawnParams {
    /// Configured options (sans `can_use_tool` and hooks; those land in
    /// `spawn` once the session id exists so the bridges can carry it).
    pub options: Options,
    /// Hooks the client wants the daemon to register on this session.
    pub hooks: Vec<WireHookSpec>,
}

impl SpawnParams {
    /// Construct from raw [`Options`] with no hook registrations. Used by
    /// tests + direct callers that don't go through the wire deserialiser.
    #[must_use]
    pub fn from_options(options: Options) -> Self {
        Self {
            options,
            hooks: Vec::new(),
        }
    }
}

impl From<Options> for SpawnParams {
    fn from(options: Options) -> Self {
        Self::from_options(options)
    }
}

// Deref so existing tests written against `Options`-returning
// `parse_spawn_params` can continue to read fields like `opts.binary`
// directly. The hooks list lives at `opts.hooks` separately.
impl std::ops::Deref for SpawnParams {
    type Target = Options;
    fn deref(&self) -> &Options {
        &self.options
    }
}

/// Extract a stable per-message identifier when one exists. Used as the
/// `event_id` field on `session.event` notifications.
///
/// `forge_sdk::Message` carries a `uuid` field on most variants but not all
/// — `Error`, `Unknown`, `System`, plus `Assistant`/`User`/`Result` when
/// the CLI hasn't been configured to emit them. Callers should treat the
/// empty string as "no id".
#[must_use]
pub fn message_event_id(msg: &forge_sdk::Message) -> &str {
    use forge_sdk::Message;
    match msg {
        Message::Assistant { uuid, .. }
        | Message::User { uuid, .. }
        | Message::Result { uuid, .. } => uuid.as_deref().unwrap_or(""),
        Message::TaskStarted { uuid, .. }
        | Message::TaskProgress { uuid, .. }
        | Message::TaskNotification { uuid, .. }
        | Message::RateLimitEvent { uuid, .. }
        | Message::StreamEvent { uuid, .. } => uuid.as_str(),
        // `System`, `Error`, `Unknown`, plus any future `non_exhaustive`
        // variants — fall through to the empty-string sentinel.
        _ => "",
    }
}

/// Wire-shape parameters for `session.send_user_message`.
#[derive(Debug, Clone, Deserialize)]
pub struct SendUserMessageParams {
    /// Session id minted by `session.spawn`.
    pub session_id: SessionId,
    /// The prompt text to forward to claude.
    pub prompt: String,
}

/// `session.send_user_message` — forward a prompt to the underlying claude.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown; `Sdk` for transport errors.
pub async fn send_user_message(
    state: &DaemonState,
    session_id: &SessionId,
    prompt: &str,
) -> Result<(), Error> {
    let handle = state
        .get_session(session_id)
        .ok_or_else(|| Error::SessionNotFound(session_id.0.clone()))?;
    let (reply, recv) = oneshot::channel();
    handle
        .commands
        .send(Command::SendUserMessage {
            prompt: prompt.to_owned(),
            reply,
        })
        .map_err(|_| Error::InternalError("session actor gone".into()))?;
    recv.await
        .map_err(|_| Error::InternalError("session actor dropped reply channel".into()))?
}

/// Result of `session.subscribe`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SubscribeResult {
    /// Number of historical messages replayed before the live cursor.
    /// M2 stub always reports 0; replay buffer lands in M3.
    pub replayed: usize,
    /// True iff the subscription is now receiving live events.
    pub live: bool,
    /// Pending prompts queued for the subscriber to answer (M4); empty in M2.
    pub pending_prompts: Vec<Value>,
}

/// Wire-shape parameters for `session.subscribe`.
#[derive(Debug, Clone, Deserialize)]
pub struct SubscribeParams {
    /// Session to subscribe to.
    pub session_id: SessionId,
    /// Resume cursor (M3); ignored in M2.
    #[serde(default)]
    pub since: Option<String>,
}

/// `session.subscribe` — register `conn` as a subscriber of `session_id`
/// and assume the primary role per D11's broadcast-with-named-primary
/// model.
///
/// **Connect = auto-primary.** The first subscriber becomes primary
/// (`reason: "initial"`); subsequent subscribers auto-takeover
/// (`reason: "auto_takeover_on_connect"`) and the previously-primary
/// client is demoted to viewer (`reason: "demoted"`). A
/// `session.role_assigned` notification fires on the new primary
/// (always) and the displaced primary (when distinct); a
/// `session.primary_changed` notification fans out to every subscriber.
///
/// Surfaces any reverse-RPC prompts currently parked in the session's
/// queue (M4) so the new client can answer them via `prompts.respond`
/// without missing prompts that arrived before they connected. Combined
/// with the takeover semantics above, this is the regression-tested
/// hand-off path: when the original primary disconnects mid-prompt and
/// a new client connects, the parked prompt surfaces here.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown.
pub fn subscribe(
    state: &DaemonState,
    conn: &crate::connection::Connection,
    session_id: &SessionId,
    _since: Option<String>,
) -> Result<SubscribeResult, Error> {
    let handle = state
        .get_session(session_id)
        .ok_or_else(|| Error::SessionNotFound(session_id.0.clone()))?;

    // Atomically (a) add conn to subscribers if not already present and
    // (b) flip the primary slot to point at conn. Capture the previous
    // primary so we can decide whether to fire role_assigned("demoted")
    // and how to label the primary_changed reason.
    let old_primary = {
        let mut subs = handle.subscribers.lock();
        if !subs.contains(&conn.id) {
            subs.push(conn.id.clone());
        }
        let mut primary_guard = handle.primary.lock();
        let prev = primary_guard.clone();
        *primary_guard = Some(conn.id.clone());
        prev
    };

    let displaced_other = matches!(&old_primary, Some(prev) if prev != &conn.id);
    let reason: &str = if displaced_other {
        "auto_takeover_on_connect"
    } else {
        "initial"
    };

    // Notify the new primary of their role. Always — even if conn was
    // already primary (re-subscribe), so clients can rely on receiving
    // a role frame after every successful subscribe.
    let _ = conn
        .outbound
        .send(crate::connection::Outbound::Notification(
            crate::jsonrpc::Notification::new(
                "session.role_assigned",
                serde_json::json!({
                    "session_id": session_id.0,
                    "role": "primary",
                    "primary": conn.id.0,
                    "reason": reason,
                }),
            ),
        ));

    // If we displaced a different primary, notify them of the demotion.
    if displaced_other {
        if let Some(old) = old_primary.as_ref() {
            // Snapshot the connection's outbound channel without
            // holding the connections lock while we send.
            let outbound = state
                .connections
                .lock()
                .get(old)
                .map(|c| c.outbound.clone());
            if let Some(out) = outbound {
                let _ = out.send(crate::connection::Outbound::Notification(
                    crate::jsonrpc::Notification::new(
                        "session.role_assigned",
                        serde_json::json!({
                            "session_id": session_id.0,
                            "role": "viewer",
                            "primary": conn.id.0,
                            "reason": "demoted",
                        }),
                    ),
                ));
            }
        }
    }

    // Broadcast primary_changed to all subscribers (viewers + new primary).
    let primary_changed =
        crate::connection::Outbound::Notification(crate::jsonrpc::Notification::new(
            "session.primary_changed",
            serde_json::json!({
                "session_id": session_id.0,
                "primary": conn.id.0,
                "previous": old_primary.as_ref().map(|c| c.0.clone()),
                "reason": reason,
            }),
        ));
    crate::broadcast::fanout(state, session_id, &primary_changed);

    let pending_views = handle.prompts.snapshot_for_wire();
    let pending_prompts = pending_views
        .into_iter()
        .map(|v| serde_json::to_value(v).map_err(Error::Json))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(SubscribeResult {
        replayed: 0,
        live: true,
        pending_prompts,
    })
}

/// Wire-shape parameters for `session.unsubscribe`.
#[derive(Debug, Clone, Deserialize)]
pub struct UnsubscribeParams {
    /// Session to detach from.
    pub session_id: SessionId,
}

/// `session.unsubscribe` — remove `conn` from the subscriber list. Clears
/// the primary slot if `conn` held it.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown.
pub fn unsubscribe(
    state: &DaemonState,
    conn: &crate::connection::Connection,
    session_id: &SessionId,
) -> Result<(), Error> {
    let handle = state
        .get_session(session_id)
        .ok_or_else(|| Error::SessionNotFound(session_id.0.clone()))?;
    handle.subscribers.lock().retain(|c| c != &conn.id);
    let mut primary = handle.primary.lock();
    if primary.as_ref() == Some(&conn.id) {
        *primary = None;
    }
    Ok(())
}

/// Wire-shape parameters for `session.disconnect`.
#[derive(Debug, Clone, Deserialize)]
pub struct DisconnectParams {
    /// Session to tear down.
    pub session_id: SessionId,
}

/// `session.disconnect` — ask the actor to consume its [`Client`] and call
/// [`Client::disconnect`]. The actor handles unregistering the session
/// once the call returns.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown; `Sdk` for transport errors.
pub async fn disconnect(state: &DaemonState, session_id: &SessionId) -> Result<(), Error> {
    let handle = state
        .get_session(session_id)
        .ok_or_else(|| Error::SessionNotFound(session_id.0.clone()))?;
    let (reply, recv) = oneshot::channel();
    handle
        .commands
        .send(Command::Disconnect { reply })
        .map_err(|_| Error::InternalError("session actor gone".into()))?;
    recv.await
        .map_err(|_| Error::InternalError("session actor dropped reply channel".into()))?
}

/// Wire-shape parameters for `session.end_input`.
#[derive(Debug, Clone, Deserialize)]
pub struct EndInputParams {
    /// Session whose stdin should be closed.
    pub session_id: SessionId,
}

/// `session.end_input` — close the subprocess's stdin so it can flush its
/// final result frame and exit. Does NOT unregister the session; the
/// actor's read loop emits `session.closed` when `next_event` returns
/// `Ok(None)` / `Err(_)` (M2.6).
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown; `Sdk` for transport errors.
pub async fn end_input(state: &DaemonState, session_id: &SessionId) -> Result<(), Error> {
    let handle = state
        .get_session(session_id)
        .ok_or_else(|| Error::SessionNotFound(session_id.0.clone()))?;
    let (reply, recv) = oneshot::channel();
    handle
        .commands
        .send(Command::EndInput { reply })
        .map_err(|_| Error::InternalError("session actor gone".into()))?;
    recv.await
        .map_err(|_| Error::InternalError("session actor dropped reply channel".into()))?
}

/// Spawn the actor task that exclusively owns `client` for the lifetime of
/// the session. The actor `select!`s between:
///
/// 1. Inbound [`Command`]s from dispatch handlers — `SendUserMessage`,
///    `EndInput`, `Disconnect` — and runs them on the [`Client`].
/// 2. Outbound `next_event` calls — fans each [`forge_sdk::Message`] to
///    the session's subscribers as a `session.event` notification.
///
/// On a terminal frame (`Message::Result`), `Ok(None)` from `next_event`,
/// or any transport error, the actor emits a `session.closed` notification
/// and unregisters the session from the daemon (M2.6).
#[allow(
    clippy::too_many_lines,
    reason = "one match arm per Command variant by design; the actor's command dispatch table is the natural shape"
)]
fn spawn_session_actor(
    state: DaemonState,
    handle: &SessionHandle,
    mut client: Client,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<Command>,
) {
    let session_id = handle.id.clone();
    tokio::spawn(async move {
        let reason: &'static str = loop {
            tokio::select! {
                biased;
                cmd = commands.recv() => {
                    let Some(cmd) = cmd else {
                        // Senders all dropped — session is being torn down.
                        break "actor_idle";
                    };
                    match cmd {
                        Command::SendUserMessage { prompt, reply } => {
                            let r = client.send_user_message(&prompt).await.map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::EndInput { reply } => {
                            let r = client.end_input().await.map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::Disconnect { reply } => {
                            let r = client.disconnect().await.map_err(Error::Sdk);
                            let _ = reply.send(r);
                            break "disconnect";
                        }
                        Command::Interrupt { reply } => {
                            let r = client.interrupt().await.map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::SetPermissionMode { mode, reply } => {
                            let r = client.set_permission_mode(mode).await.map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::SetModel { model, reply } => {
                            let r = client
                                .set_model(model.as_deref())
                                .await
                                .map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::RewindFiles { user_message_id, reply } => {
                            let r = client
                                .rewind_files(&user_message_id)
                                .await
                                .map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::StopTask { task_id, reply } => {
                            let r = client.stop_task(&task_id).await.map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::McpStatus { reply } => {
                            let r = client.mcp_status().await.map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::McpReconnect { server_name, reply } => {
                            let r = client
                                .mcp_reconnect(&server_name)
                                .await
                                .map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::McpToggle { server_name, enabled, reply } => {
                            let r = client
                                .mcp_toggle(&server_name, enabled)
                                .await
                                .map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::ContextGet { reply } => {
                            let r = client.get_context_usage().await.map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                    }
                }
                next = client.next_event() => {
                    match next {
                        Ok(Some(msg)) => {
                            let is_terminal = matches!(msg, forge_sdk::Message::Result { .. });
                            let event_id = message_event_id(&msg).to_owned();
                            let frame = crate::connection::Outbound::Notification(
                                crate::jsonrpc::Notification::new(
                                    "session.event",
                                    serde_json::json!({
                                        "session_id": session_id.0,
                                        "event_id": event_id,
                                        "message": msg,
                                    }),
                                ),
                            );
                            crate::broadcast::fanout(&state, &session_id, &frame);
                            if is_terminal {
                                break "result_frame";
                            }
                        }
                        Ok(None) => break "disconnected",
                        Err(e) => {
                            tracing::warn!(session_id = %session_id.0, error = %e, "next_event error");
                            break "error";
                        }
                    }
                }
            }
        };

        // Drain any parked prompts before unregistering — otherwise
        // SDK callbacks awaiting on parked oneshots wait the full 1h
        // timeout. Each parked prompt gets a synthetic
        // `_session_closed: true` answer so the bridge unblocks
        // immediately, plus a `prompts.expired` broadcast so any
        // subscribers know the prompt is gone.
        crate::reverse_rpc::drain_prompts_on_session_exit(&state, &session_id);

        // Emit session.closed to all subscribers.
        let closed = crate::connection::Outbound::Notification(crate::jsonrpc::Notification::new(
            "session.closed",
            serde_json::json!({
                "session_id": session_id.0,
                "reason": reason,
            }),
        ));
        crate::broadcast::fanout(&state, &session_id, &closed);

        // Unregister the session — frees state, decrements active_sessions.
        state.unregister_session(&session_id);
    });
}

// =============================================================================
// session.spawn — full Options deserialiser (M3.1)
// =============================================================================

/// Wire-shape mirror of [`forge_sdk::Options`]. Lifted from the public
/// SDK surface and decoupled from it: when the SDK adds a field we add
/// it here too; when the SDK drops a field we drop it here and document
/// the back-compat in the changelog.
///
/// Fields the daemon does NOT accept on the wire (because they have no
/// meaningful JSON representation across the boundary — function
/// callbacks, stderr handlers, in-process MCP server handles) are
/// silently ignored. The deserialiser uses `deny_unknown_fields` so
/// typos in supported field names surface as errors rather than being
/// silently dropped.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors Options field-for-field; mirroring a foreign struct's bool flags is intentional"
)]
struct WireOptions {
    binary: Option<String>,
    cwd: Option<String>,
    resume: Option<String>,
    model: Option<String>,
    permission_mode: Option<String>,
    allowed_tools: Vec<String>,
    skills: Vec<String>,
    setting_sources: Option<Vec<String>>,
    exclude_dynamic_sections: Option<bool>,
    permission_prompt_tool_name: Option<String>,
    minimum_cli_version: Option<String>,
    projects_dir: Option<String>,
    system_prompt: Option<WireSystemPrompt>,
    tools: Option<WireTools>,
    disallowed_tools: Vec<String>,
    max_turns: Option<u64>,
    max_budget_usd: Option<f64>,
    fallback_model: Option<String>,
    betas: Vec<String>,
    continue_conversation: bool,
    session_id: Option<String>,
    include_partial_messages: bool,
    fork_session: bool,
    add_dirs: Vec<String>,
    plugins: Vec<WirePlugin>,
    env: std::collections::HashMap<String, String>,
    user: Option<String>,
    extra_args: std::collections::HashMap<String, Option<String>>,
    effort: Option<WireEffort>,
    thinking: Option<WireThinking>,
    max_thinking_tokens: Option<u64>,
    task_budget: Option<u64>,
    output_format: Option<serde_json::Value>,
    max_buffer_size: Option<usize>,
    enable_file_checkpointing: bool,
    settings: Option<String>,
    /// Hook registrations (M4). Each entry attaches a
    /// [`ForgedHookBridge`](crate::sdk_callbacks::ForgedHookBridge) for
    /// the given hook kind so the CLI's hook callbacks fan out over
    /// reverse-RPC.
    hooks: Vec<WireHookSpec>,
}

/// System-prompt wire shape. Mirrors [`forge_sdk::SystemPromptKind`].
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireSystemPrompt {
    /// `--system-prompt <text>`
    Inline {
        /// The literal prompt text.
        text: String,
    },
    /// `--system-prompt-file <path>`
    File {
        /// Path to the prompt file.
        path: String,
    },
    /// Preset (`claude_code`) with optional append text.
    Preset {
        /// Optional append payload — `--append-system-prompt <text>`.
        #[serde(default)]
        append: Option<String>,
    },
}

/// Plugin wire shape. Mirrors [`forge_sdk::SdkPluginConfig`].
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WirePlugin {
    /// Local filesystem plugin.
    Local {
        /// Directory containing the plugin.
        path: String,
    },
}

/// Tools-preset wire shape. Mirrors [`forge_sdk::ToolsPreset`].
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireTools {
    /// `--tools default`
    Default,
    /// Explicit `--tools <csv>` list.
    List {
        /// The tool list to forward.
        tools: Vec<String>,
    },
}

/// Thinking-config wire shape. Mirrors [`forge_sdk::ThinkingConfig`].
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireThinking {
    /// CLI picks per-turn.
    Adaptive,
    /// Thinking on with a per-turn token cap.
    Enabled {
        /// Per-turn budget.
        budget_tokens: u64,
    },
    /// Thinking off.
    Disabled,
}

/// Effort wire shape — string preset or numeric override.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
enum WireEffort {
    /// `low | medium | high | max`
    Preset(String),
    /// Numeric override.
    Numeric(i64),
}

/// Parse the full `session.spawn` params into a configured
/// [`forge_sdk::Options`] plus the [`WireHookSpec`] list. Replaces the
/// M2 stub.
///
/// Hooks are returned separately rather than baked into Options
/// because their attachment depends on the session id (each
/// [`ForgedHookBridge`](crate::sdk_callbacks::ForgedHookBridge) carries
/// the session id as a field), and the session id is minted by `spawn`
/// itself.
///
/// # Errors
///
/// [`Error::InvalidParams`] when the `options` blob fails serde
/// deserialisation, references an unknown enum variant (e.g. an
/// unrecognised `permission_mode`), or carries an unknown field.
#[allow(
    clippy::too_many_lines,
    reason = "one builder call per Options field by design; collapsing would obscure the wire-shape mapping"
)]
pub fn parse_spawn_params(raw: &Value) -> Result<SpawnParams, Error> {
    let opts_v = raw
        .get("options")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let wire: WireOptions = serde_json::from_value(opts_v)
        .map_err(|e| Error::InvalidParams(format!("options: {e}")))?;
    let hook_specs = wire.hooks.clone();

    let mut b = OptionsBuilder::new();
    if let Some(bin) = wire.binary {
        b = b.binary(bin);
    }
    if let Some(cwd) = wire.cwd {
        b = b.cwd(cwd);
    }
    if let Some(model) = wire.model {
        b = b.model(model);
    }
    if let Some(resume) = wire.resume {
        b = b.resume(resume);
    }
    if let Some(mode_str) = wire.permission_mode.as_deref() {
        let mode = match mode_str {
            "ask" => PermissionMode::Ask,
            "accept_edits" => PermissionMode::AcceptEdits,
            "plan" => PermissionMode::Plan,
            "bypass_permissions" => PermissionMode::BypassPermissions,
            "auto" => PermissionMode::Auto,
            "deny_permissions" => PermissionMode::DenyPermissions,
            other => {
                return Err(Error::InvalidParams(format!(
                    "permission_mode: unknown variant '{other}'"
                )));
            }
        };
        b = b.permission_mode(mode);
    }
    if !wire.allowed_tools.is_empty() {
        b = b.allowed_tools(wire.allowed_tools);
    }
    if !wire.disallowed_tools.is_empty() {
        b = b.disallowed_tools(wire.disallowed_tools);
    }
    if !wire.skills.is_empty() {
        b = b.skills(wire.skills);
    }
    if let Some(sources) = wire.setting_sources {
        b = b.setting_sources(sources);
    }
    if let Some(v) = wire.exclude_dynamic_sections {
        b = b.exclude_dynamic_sections(v);
    }
    if let Some(name) = wire.permission_prompt_tool_name {
        b = b.permission_prompt_tool_name(name);
    }
    if let Some(min) = wire.minimum_cli_version {
        b = b.minimum_cli_version(Some(min));
    }
    if let Some(d) = wire.projects_dir {
        b = b.projects_dir(d);
    }
    if let Some(sp) = wire.system_prompt {
        let kind = match sp {
            WireSystemPrompt::Inline { text } => SystemPromptKind::Inline(text),
            WireSystemPrompt::File { path } => SystemPromptKind::File(path.into()),
            WireSystemPrompt::Preset { append } => SystemPromptKind::Preset {
                append,
                exclude_dynamic_sections: None,
            },
        };
        b = b.system_prompt(kind);
    }
    if let Some(t) = wire.tools {
        let preset = match t {
            WireTools::Default => forge_sdk::ToolsPreset::Default,
            WireTools::List { tools } => forge_sdk::ToolsPreset::List(tools),
        };
        b = b.tools(preset);
    }
    if let Some(n) = wire.max_turns {
        b = b.max_turns(n);
    }
    if let Some(n) = wire.max_budget_usd {
        b = b.max_budget_usd(n);
    }
    if let Some(m) = wire.fallback_model {
        b = b.fallback_model(m);
    }
    if !wire.betas.is_empty() {
        b = b.betas(wire.betas);
    }
    if wire.continue_conversation {
        b = b.continue_conversation(true);
    }
    if let Some(sid) = wire.session_id {
        b = b.session_id(sid);
    }
    if wire.include_partial_messages {
        b = b.include_partial_messages(true);
    }
    if wire.fork_session {
        b = b.fork_session(true);
    }
    if !wire.add_dirs.is_empty() {
        b = b.add_dirs(wire.add_dirs.into_iter().map(Into::into).collect());
    }
    if !wire.plugins.is_empty() {
        let plugins: Vec<SdkPluginConfig> = wire
            .plugins
            .into_iter()
            .map(|p| match p {
                WirePlugin::Local { path } => SdkPluginConfig::Local { path: path.into() },
            })
            .collect();
        b = b.plugins(plugins);
    }
    if !wire.env.is_empty() {
        b = b.envs(wire.env);
    }
    if let Some(u) = wire.user {
        b = b.user(u);
    }
    for (k, v) in wire.extra_args {
        b = b.extra_arg(k, v);
    }
    if let Some(eff) = wire.effort {
        let level = match eff {
            WireEffort::Preset(s) => match s.as_str() {
                "low" => EffortLevel::Preset(EffortPreset::Low),
                "medium" => EffortLevel::Preset(EffortPreset::Medium),
                "high" => EffortLevel::Preset(EffortPreset::High),
                "max" => EffortLevel::Preset(EffortPreset::Max),
                other => {
                    return Err(Error::InvalidParams(format!(
                        "effort: unknown preset '{other}'"
                    )));
                }
            },
            WireEffort::Numeric(n) => EffortLevel::Numeric(n),
        };
        b = b.effort(level);
    }
    if let Some(t) = wire.thinking {
        let cfg = match t {
            WireThinking::Adaptive => ThinkingConfig::Adaptive,
            WireThinking::Enabled { budget_tokens } => ThinkingConfig::Enabled { budget_tokens },
            WireThinking::Disabled => ThinkingConfig::Disabled,
        };
        b = b.thinking(cfg);
    }
    if let Some(t) = wire.max_thinking_tokens {
        b = b.max_thinking_tokens(t);
    }
    if let Some(t) = wire.task_budget {
        b = b.task_budget(t);
    }
    if let Some(v) = wire.output_format {
        b = b.output_format(v);
    }
    if let Some(n) = wire.max_buffer_size {
        b = b.max_buffer_size(n);
    }
    if wire.enable_file_checkpointing {
        b = b.enable_file_checkpointing(true);
    }
    if let Some(s) = wire.settings {
        b = b.settings(s);
    }

    Ok(SpawnParams {
        options: b.build(),
        hooks: hook_specs,
    })
}

// =============================================================================
// Mid-session control (M3.6) — thin command-senders into the actor task.
// =============================================================================

/// `session.interrupt` — send the actor an [`Command::Interrupt`] and
/// await its acknowledgement.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown; `Sdk` for transport errors.
pub async fn interrupt(state: &DaemonState, session_id: &SessionId) -> Result<(), Error> {
    let handle = state
        .get_session(session_id)
        .ok_or_else(|| Error::SessionNotFound(session_id.0.clone()))?;
    let (reply, recv) = oneshot::channel();
    handle
        .commands
        .send(Command::Interrupt { reply })
        .map_err(|_| Error::InternalError("session actor gone".into()))?;
    recv.await
        .map_err(|_| Error::InternalError("session actor dropped reply channel".into()))?
}

/// `session.set_permission_mode` — switch the permission flow mid-session.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown; `Sdk` for transport errors.
pub async fn set_permission_mode(
    state: &DaemonState,
    session_id: &SessionId,
    mode: PermissionMode,
) -> Result<(), Error> {
    let handle = state
        .get_session(session_id)
        .ok_or_else(|| Error::SessionNotFound(session_id.0.clone()))?;
    let (reply, recv) = oneshot::channel();
    handle
        .commands
        .send(Command::SetPermissionMode { mode, reply })
        .map_err(|_| Error::InternalError("session actor gone".into()))?;
    recv.await
        .map_err(|_| Error::InternalError("session actor dropped reply channel".into()))?
}

/// `session.set_model` — switch the active model mid-session. `None`
/// reverts to the CLI default.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown; `Sdk` for transport errors.
pub async fn set_model(
    state: &DaemonState,
    session_id: &SessionId,
    model: Option<String>,
) -> Result<(), Error> {
    let handle = state
        .get_session(session_id)
        .ok_or_else(|| Error::SessionNotFound(session_id.0.clone()))?;
    let (reply, recv) = oneshot::channel();
    handle
        .commands
        .send(Command::SetModel { model, reply })
        .map_err(|_| Error::InternalError("session actor gone".into()))?;
    recv.await
        .map_err(|_| Error::InternalError("session actor dropped reply channel".into()))?
}

/// `session.rewind_files` — ask the CLI to revert file edits since the
/// supplied user message.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown; `Sdk` for transport errors.
pub async fn rewind_files(
    state: &DaemonState,
    session_id: &SessionId,
    user_message_id: String,
) -> Result<(), Error> {
    let handle = state
        .get_session(session_id)
        .ok_or_else(|| Error::SessionNotFound(session_id.0.clone()))?;
    let (reply, recv) = oneshot::channel();
    handle
        .commands
        .send(Command::RewindFiles {
            user_message_id,
            reply,
        })
        .map_err(|_| Error::InternalError("session actor gone".into()))?;
    recv.await
        .map_err(|_| Error::InternalError("session actor dropped reply channel".into()))?
}

/// `session.stop_task` — kill an in-flight sub-agent task.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown; `Sdk` for transport errors.
pub async fn stop_task(
    state: &DaemonState,
    session_id: &SessionId,
    task_id: String,
) -> Result<(), Error> {
    let handle = state
        .get_session(session_id)
        .ok_or_else(|| Error::SessionNotFound(session_id.0.clone()))?;
    let (reply, recv) = oneshot::channel();
    handle
        .commands
        .send(Command::StopTask { task_id, reply })
        .map_err(|_| Error::InternalError("session actor gone".into()))?;
    recv.await
        .map_err(|_| Error::InternalError("session actor dropped reply channel".into()))?
}
