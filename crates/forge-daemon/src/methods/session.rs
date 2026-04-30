//! `session.*` method handlers.
//!
//! Each handler is a thin proxy over the session's actor task — they
//! enqueue a [`Command`] on the session's mpsc and `.await` the actor's
//! reply. The actor (spawned at `session.spawn`) is the sole owner of
//! the [`forge_sdk::Client`]; locking the [`Client`] from multiple
//! tasks would deadlock because [`forge_sdk::Client::next_event`]
//! holds `&mut self` across subprocess I/O.

mod actor;
mod wire_options;

pub use wire_options::parse_spawn_params;

use std::sync::Arc;

use forge_sdk::{Client, Options, PermissionMode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::info;
use uuid::Uuid;

use crate::Error;
use crate::registry::DaemonState;
use crate::sdk_callbacks::WireHookSpec;
use crate::session_state::dispatch_command;
use crate::session_state::{Command, SessionId};

use self::actor::spawn_session_actor;

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
/// The SDK's [`Subprocess`](forge_sdk::transport::process::Subprocess)
/// drives subprocess I/O over internal mpsc channels, so
/// [`Client::next_event`] is cancel-safe and the SDK's reader task
/// internally `tokio::spawn`s `control_request` dispatch on detached
/// tasks via a clonable writer. The session actor just runs a plain
/// `select!` between command and `next_event` — see the actor module
/// for the loop shape.
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
    let SpawnParams { options, hooks } = params.into();
    let session_id = SessionId(format!("sess_{}", Uuid::new_v4()));
    spawn_with_id(state, session_id.clone(), options, hooks).await?;
    Ok(SpawnResult { session_id })
}

/// Internal spawn helper — same body as [`spawn`] but with a
/// caller-provided session id and an unwrapped return shape. Lets
/// [`subscribe`] auto-resume an on-disk transcript and register it
/// under the historical UUID so the subscriber doesn't need to chase a
/// freshly-minted id.
pub(crate) async fn spawn_with_id(
    state: &DaemonState,
    session_id: SessionId,
    mut options: Options,
    hook_specs: Vec<WireHookSpec>,
) -> Result<(), Error> {
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

    let client = Client::spawn(options).await.map_err(Error::Sdk)?;
    let (handle, rx) = state.register_session(session_id.clone());
    spawn_session_actor(state.clone(), &handle, client, rx);
    info!(session_id = %session_id.0, "session spawned");
    Ok(())
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

impl From<Options> for SpawnParams {
    fn from(options: Options) -> Self {
        Self {
            options,
            hooks: Vec::new(),
        }
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

/// Soft cap on `session.send_user_message` prompt size. Defensive
/// hygiene against accidentally pasting a giant blob — even in the
/// single-user trust model the prompt is held in three places (incoming
/// WS buffer, parsed `Request::params`, `Command` enum) before reaching
/// claude's stdin, so the multiplier matters. 1 MiB is well above
/// realistic prompt sizes.
const MAX_PROMPT_BYTES: usize = 1 << 20;

/// `session.send_user_message` — forward a prompt to the underlying claude.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown; `Sdk` for transport errors;
/// `InvalidParams` if the prompt exceeds the internal 1 MiB cap.
pub async fn send_user_message(
    state: &DaemonState,
    session_id: &SessionId,
    prompt: &str,
) -> Result<(), Error> {
    if prompt.len() > MAX_PROMPT_BYTES {
        return Err(Error::InvalidParams(format!(
            "prompt: exceeds {MAX_PROMPT_BYTES} bytes (got {})",
            prompt.len()
        )));
    }
    dispatch_command(state, session_id, |reply| Command::SendUserMessage {
        prompt: prompt.to_owned(),
        reply,
    })
    .await
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
pub async fn subscribe(
    state: &DaemonState,
    conn: &crate::connection::Connection,
    session_id: &SessionId,
    since: Option<&str>,
) -> Result<SubscribeResult, Error> {
    // M5 stub — no replay buffer is implemented yet, so any caller that
    // requests resume gets a typed ReplayUnavailable. Clients should
    // refetch via sessions.messages and fall through to live mode. Once
    // the replay buffer lands, this branch turns into "find offset, replay
    // the events" and `buffer_window_seconds` will reflect the configured
    // retention.
    if since.is_some() {
        return Err(Error::ReplayUnavailable {
            buffer_window_seconds: 0,
        });
    }

    // Auto-resume on subscribe-miss: if the id isn't an active session
    // but matches an on-disk transcript, resume it under the same id.
    // The cwd from the on-disk session info is critical — `claude
    // --resume <sid>` looks the session up under the project keyed by
    // cwd, so a daemon-default cwd (whatever the daemon was launched
    // from) makes claude bail with "session not found" and close
    // stdout before the initialize control_response, surfacing as
    // forge_sdk::Error::Connection (-32101).
    if state.get_session(session_id).is_none()
        && let Some(info) = forge_sdk::session::scan::get_session_info(&session_id.0, None)
    {
        info!(session_id = %session_id.0, "subscribe: auto-resuming on-disk session");
        let mut builder = forge_sdk::OptionsBuilder::new().resume(session_id.0.clone());
        if let Some(cwd) = info.cwd.as_deref() {
            builder = builder.cwd(cwd);
        }
        // Forward claude's stderr to operator logs so spawn
        // failures (CLI version mismatch, missing binary,
        // resume-target rejected) show up in events.log instead
        // of vanishing.
        let sid_for_log = session_id.0.clone();
        let options = builder
            .stderr(move |line| {
                tracing::warn!(session_id = %sid_for_log, "claude stderr: {line}");
            })
            .build();
        spawn_with_id(state, session_id.clone(), options, Vec::new()).await?;
    }

    let handle = state
        .get_session(session_id)
        .ok_or_else(|| Error::SessionNotFound(session_id.0.clone()))?;

    // Use the shared `become_primary` helper so subscribe and claim
    // share the same atomic-swap + notification trio. Subscribe's
    // distinguishing details: the role/broadcast reasons depend on
    // whether the slot was empty (initial) or held by a different conn
    // (auto_takeover_on_connect).
    crate::methods::multi_client::become_primary(
        state,
        &handle,
        session_id,
        &conn.id,
        "initial",                  // role_reason_initial
        "auto_takeover_on_connect", // role_reason_takeover
        "initial",                  // broadcast_reason_initial
        "auto_takeover_on_connect", // broadcast_reason_takeover
        "subscribe",
    );

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
    dispatch_command(state, session_id, |reply| Command::Disconnect { reply }).await
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
    dispatch_command(state, session_id, |reply| Command::EndInput { reply }).await
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
    dispatch_command(state, session_id, |reply| Command::Interrupt { reply }).await
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
    dispatch_command(state, session_id, |reply| Command::SetPermissionMode {
        mode,
        reply,
    })
    .await
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
    dispatch_command(state, session_id, |reply| Command::SetModel {
        model,
        reply,
    })
    .await
}

/// Result shape for `session.current_model`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CurrentModelResult {
    /// Active model id (e.g. `"claude-opus-4-7[1m]"`). `None` when the
    /// CLI hasn't reported a model yet (rare — happens between
    /// session.spawn and the first system/init frame).
    pub model: Option<String>,
}

/// `session.current_model` — return the model id captured from the
/// CLI's `system/init` payload, kept in sync with `session.set_model`
/// updates. Used by forge-tui's footer poller.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown.
pub async fn current_model(
    state: &DaemonState,
    session_id: &SessionId,
) -> Result<CurrentModelResult, Error> {
    let model =
        dispatch_command(state, session_id, |reply| Command::CurrentModel { reply }).await?;
    Ok(CurrentModelResult { model })
}

/// One slash-command entry in `slash.list`. Mirrors the CLI's
/// `init.slash_commands[*]` shape (subset).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SlashCommandEntry {
    /// Command name, e.g. `"help"` (no leading slash).
    pub name: String,
    /// Human-readable description from the CLI's catalog.
    pub description: String,
}

/// Result shape for `slash.list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SlashListResult {
    /// Slash commands the CLI advertises in its `system/init` payload.
    pub commands: Vec<SlashCommandEntry>,
}

/// `slash.list` — return the slash-command catalog from the CLI's
/// init payload. Forge-tui uses this for autocomplete; the actual
/// dispatch happens by sending `/<name> <args>` as a user message,
/// which the CLI parses and runs internally.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown.
pub async fn slash_list(
    state: &DaemonState,
    session_id: &SessionId,
) -> Result<SlashListResult, Error> {
    let pairs = dispatch_command(state, session_id, |reply| Command::SlashList { reply }).await?;
    let commands = pairs
        .into_iter()
        .map(|(name, description)| SlashCommandEntry { name, description })
        .collect();
    Ok(SlashListResult { commands })
}

/// Account info captured from the CLI's `system/init` payload. CLI
/// emits `camelCase` fields; daemon RPC normalises to `snake_case`
/// for forge-tui's `AccountInfo` struct. Every field is optional —
/// the CLI may omit any of them depending on auth source.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AccountSnapshot {
    /// Logged-in user email when first-party OAuth is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Organization the account belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    /// Subscription tier label (e.g. `"team"`, `"enterprise"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription_type: Option<String>,
    /// Where the auth token came from (`"oauth"`, `"api_key"`, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_source: Option<String>,
    /// Where the API key was loaded from (`"environment"`, `"keychain"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_source: Option<String>,
    /// Active backend: `"firstParty" | "bedrock" | "vertex" | "foundry" | "anthropicAws" | "mantle"`.
    /// Anthropic OAuth fields only populate when this is `"firstParty"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_provider: Option<String>,
}

/// `session.status_snapshot` — return the account info the CLI
/// published in its `system/init` payload. Pure init-data read; no
/// `control_request` is issued.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown.
pub async fn status_snapshot(
    state: &DaemonState,
    session_id: &SessionId,
) -> Result<AccountSnapshot, Error> {
    dispatch_command(state, session_id, |reply| Command::StatusSnapshot { reply }).await
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
    dispatch_command(state, session_id, |reply| Command::RewindFiles {
        user_message_id,
        reply,
    })
    .await
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
    dispatch_command(state, session_id, |reply| Command::StopTask {
        task_id,
        reply,
    })
    .await
}
