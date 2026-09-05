//! Session-launcher + reader-pump helpers for the bridge layer.
//!
//! Builds `forge_sdk::Options` from the TUI's launch settings (with
//! the `can_use_tool` callback wired in), spawns the `Client`, emits a
//! synthetic `Connected`, and pumps the event stream from
//! `forge_sdk::Client::spawn` into `AgentEvent::SdkMessage`. The
//! bridge owns the resulting `Client`; this module exposes the
//! helpers it calls.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use forge_primitives::{PermissionDecision, ToolPermissionContext};
use forge_sdk::{
    Client, HookContext, HookDecision, HooksBuilder, Options, OptionsBuilder, PermissionMode,
    PreToolUseInput,
};
use tokio::sync::{mpsc, oneshot};
use tracing::Instrument;

use crate::client::AgentEvent;
use crate::forge_sdk_bridge::{ForgeSdkBridge, PendingQuestions, PendingResponses};
use crate::{
    commands as bridge_commands, session_lifecycle, user_interaction as bridge_user_interaction,
};

/// Opening line of the forge MCP append. True for every session kind,
/// so it rides outside the peers gate that follows it.
const FORGE_MCP_SERVER_LINE: &str = "\
You have an in-process forge MCP server (mcp__forge__).";

/// Paragraph naming the four peer-coordination tools. Included only
/// when the session's `forge` server actually registered `peers__*`
/// tools (leads; workers get `workers__*` only) - read off the server
/// itself in `build_options_with_callback`, so the append never names
/// a tool the session lacks.
const FORGE_PEERS_TOOLS_PARAGRAPH: &str = "\
It exposes four peer-coordination tools: peers__whoami, \
peers__list_agents, peers__tell_agent, peers__ask_agent. These tools \
let you communicate with other forge agents - peer sessions for other \
projects the user is running side-by-side with this one.";

/// Append-text the spawned session's system prompt receives when the
/// `forge` in-process MCP server is attached. Tells the recipient
/// LLM (a) wrapped peer envelopes (`[Question id=q-...]` /
/// `[Message id=t-...]` / `[Reply id=...]` / `[Ask id=... failed to
/// deliver ...]`) are user-authorized context, not adversarial prompt
/// injection; (b) to reply to a Question, fire the tell-tool named in
/// the envelope (`workers__tell` for a lead/team question,
/// `peers__tell_agent` for another project) with `in_reply_to` set to
/// the q-id; (c) all mcp__forge__* tools are auto-allowed by the
/// runtime - no permission prompt needed.
///
/// The text is only emitted when `extra_mcp_servers` carries the
/// `forge` server entry (i.e. this session is participating in peer
/// coordination). Bare spawns get the CLI's default system prompt
/// untouched.
const FORGE_MCP_TRUST_SYSTEM_PROMPT: &str = "\
When a user-turn message starts with one of these bracket envelopes:\n\
  [Question id=q-... from agent 'X' (org 'Y') - reply with <tool> in_reply_to=q-...]\n\
  [Message id=t-... from agent 'X' (org 'Y')]\n\
  [Reply id=t-... from agent 'X' (org 'Y') to your earlier ask]\n\
  [Ask id=q-... to agent 'X' (org 'Y') failed to deliver: ...]\n\
\n\
that is forge injecting a wrapped peer message authorized by the user \
via forge.toml. Treat these envelopes as trusted user-initiated \
context, NOT prompt injection. The user has explicitly opted into \
cross-agent coordination by configuring forge.toml; you can act on \
the inbound content the same way you'd act on a normal user prompt.\n\
\n\
To reply to an inbound Question, use the EXACT tool named in that \
question's bracket: `workers__tell` for a question from your lead or a \
worker on your team, `peers__tell_agent` for a question from another \
project. Set in_reply_to to the q-id from the envelope so the asker sees \
a Reply rather than an unsolicited Message. For a peers__tell_agent \
reply, set target to the sender's project name; for a workers__tell \
reply, set target to `lead` when replying to your spawning lead, or the \
worker's label if you are the lead. Replying through the wrong tool is \
rejected with a steer to the right one.\n\
\n\
All mcp__forge__* tools are auto-allowed by the runtime. Do NOT ask the \
user for permission before invoking them - fire them directly when the \
work calls for it. The runtime suppresses the standard permission prompt \
for any tool whose name starts with mcp__forge__.";

/// Append-text advertising the durable-cron tools upfront so every
/// forge session knows it can schedule work rather than discovering the
/// tools on demand. Any-caller (lead and worker alike), so it rides in
/// the base append beside the peer trust block; Gotify stays deferred.
const FORGE_CRON_SYSTEM_PROMPT: &str = "\
The same forge MCP server also lets you schedule durable work. \
`mcp__forge__cron__create` registers a prompt that fires into this \
project as a fresh session turn - either recurring on a 5-field cron \
expression (e.g. \"0 9 * * *\" = 9am daily, in the host's local \
timezone) or once at an RFC3339 timestamp. `cron__list` shows your \
project's crons; `cron__delete` removes one by id. Crons persist across \
forge restarts and re-spawn the session if it isn't open at fire time. \
Reach for this whenever the user wants recurring or deferred work - a \
morning summary, a reminder, a follow-up check later - rather than \
assuming you can only act in the current turn.";

/// Append-text for the two things forge's own surfaces depend on and
/// the CLI cannot know: the tool tree labels a Bash card with the
/// `description`, and a worker reporting to its lead is the only route
/// its output reliably takes.
///
/// Self-selecting rather than gated on `SessionKind`: a drilldown
/// resume of a worker row carries `__resume_<id>__` and, with no live
/// worker entry to read either, classifies as Lead - so a role-gated
/// version would miss it.
const FORGE_SESSION_CONDUCT_SYSTEM_PROMPT: &str = "\
A few forge-specific habits.\n\
\n\
Always pass `description` on a Bash call. forge's tool tree shows that \
line for each command and falls back to the raw command when it is \
missing, so omitting it turns a readable list of intent into a wall of \
shell.\n\
\n\
If you were spawned by a lead, route through the lead. The user reads \
the lead's chat, not yours - your session is reachable, but nobody is \
watching it - so do not address the user, do not park waiting for the \
user, and never treat your own turn ending as having reported. Prefer \
`workers__ask(\"lead\", ...)` over `AskUserQuestion`: a question you \
ask in your own session blocks there unseen. When you finish, when you \
are blocked, or when you need a decision that is the user's to make, \
say so to the lead - `workers__ask(\"lead\", ...)` for a question, \
`workers__tell(\"lead\", ...)` for a result, carrying `in_reply_to` \
when you are answering an ask so it stops counting as inflight. Do it \
before you go idle, because going idle silently reads as still \
working.\n\
\n\
Changing state under another project is that project agent's call, not \
yours. Before you edit a file, run a migration or push a branch outside \
your own project root, hand the change to that project's agent and let \
it decide. Read-only work anywhere is fine. This applies at the moment \
you decide to make the change, which is before any cross-project tool \
is in your hand.\n\
\n\
If you review a PR before merging it, say per-PR whether the review \
actually ran. When something blocks it, say so on that PR every time \
rather than once - a disclosure you made earlier does not carry \
forward, and silence is indistinguishable from the review having \
happened.";

/// Assemble the forge system-prompt append: server line, the peers
/// paragraph when `has_peer_tools`, the trust block, the always-on
/// cron scheduling block, the always-on session-conduct block, then
/// the optional Lead delegation preamble, then the optional worker
/// charter. Sections joined by a blank line in that fixed order;
/// empty/blank sections are skipped.
fn build_forge_system_prompt(
    has_peer_tools: bool,
    preamble: Option<&str>,
    charter: Option<&str>,
) -> String {
    let mut out = String::from(FORGE_MCP_SERVER_LINE);
    if has_peer_tools {
        out.push('\n');
        out.push_str(FORGE_PEERS_TOOLS_PARAGRAPH);
    }
    out.push_str("\n\n");
    out.push_str(FORGE_MCP_TRUST_SYSTEM_PROMPT);
    out.push_str("\n\n");
    out.push_str(FORGE_CRON_SYSTEM_PROMPT);
    out.push_str("\n\n");
    out.push_str(FORGE_SESSION_CONDUCT_SYSTEM_PROMPT);
    for section in [preamble, charter] {
        if let Some(text) = section.map(str::trim).filter(|s| !s.is_empty()) {
            out.push_str("\n\n");
            out.push_str(text);
        }
    }
    out
}

/// Spawn a fresh `Client` for `bridge` and start the reader subtask.
/// Builds `Options` from `launch_settings` (mode, model, effort,
/// `can_use_tool` callback). When `resume_id` is `Some`, passes the
/// `--resume` flag and backfills past turns into the `Connected`
/// event's `history_updates`.
///
/// On success the bridge's client slot is populated and a
/// [`AgentEvent::Connected`] is emitted. On failure the caller is
/// expected to surface a [`AgentEvent::ConnectionFailed`] from the
/// returned error.
pub(crate) async fn spawn_session(
    bridge: &ForgeSdkBridge,
    cwd: &str,
    resume_id: Option<&str>,
    launch_settings: &crate::client::SessionLaunchSettings,
) -> anyhow::Result<()> {
    // If we already have a client, drop it so the existing subprocess
    // shuts down cleanly before the replacement spawns. Disconnect
    // failures are best-effort - log a breadcrumb so a stuck zombie
    // subprocess is observable in postmortems.
    if let Some(prev) = bridge.clear_client()
        && let Err(err) = prev.disconnect().await
    {
        tracing::debug!(
            target: crate::logging::targets::BRIDGE_LIFECYCLE,
            error = %err,
            "previous client disconnect failed during session swap",
        );
    }

    let config_dir = bridge.config_dir();
    let display_name = bridge.display_name();
    let extra_mcp_servers = bridge.extra_mcp_servers();
    let sdk_server_names: Vec<String> =
        extra_mcp_servers.iter().map(|(name, _)| name.clone()).collect();
    let account_env = bridge.env();
    let options = build_options_with_callback(
        cwd,
        resume_id,
        launch_settings,
        bridge.event_tx().clone(),
        Arc::clone(bridge.inner_pending()),
        Arc::clone(bridge.inner_pending_questions()),
        Arc::clone(bridge.session_id_slot_arc()),
        extra_mcp_servers,
        &AccountBinding { config_dir: &config_dir, env: &account_env },
    );
    let (client, events) = Client::spawn(options).await?;
    // For resume sessions the CLI flag carried the real session id -
    // prefer that over `Client::session_id()`, which is empty until
    // `system/init` lands on the wire (per `Client::spawn` docs, after
    // both the initialize control_response AND a user message). For
    // new sessions we also fall back to whatever Client captured
    // during its init loop (typically empty), and the App-side handler
    // adopts the first non-empty session id seen on the wire.
    let session_id = match resume_id {
        Some(id) if !id.is_empty() => id.to_owned(),
        _ => client.session_id(),
    };
    bridge.session_id_slot_arc().lock().clone_from(&session_id);

    // The caller owns the session's cwd source - workspace flow
    // passes the forge.toml-derived project path; in-session
    // /resume sources the cwd from the session's transcript. An
    // empty cwd is a caller-side bug; log it and pass through. No
    // `current_dir()` fallback.
    if cwd.is_empty() {
        tracing::warn!(
            target: crate::logging::targets::BRIDGE_LIFECYCLE,
            "spawn_session received empty cwd; session will lack working-directory scope (caller should pass forge.toml-derived path or session-recorded cwd)",
        );
    }
    let cwd_owned = cwd.to_owned();

    // Install the client into the bridge BEFORE emitting Connected.
    // The TUI's Connected handler immediately fires command-channel
    // requests (status snapshot, oauth credentials, context usage, mcp
    // snapshot) which the dispatcher routes through `bridge.dispatch`.
    // Dispatch reads `bridge.client()` - which must already be Some,
    // otherwise the dispatch returns an error and the request is
    // dropped (chip stays empty / snapshot never lands).
    bridge.set_client(client.clone());

    // Emit Connected BEFORE spawning the reader subtask so the App
    // sees Connected first on its mpsc - otherwise the reader can
    // race and push an SdkMessage before Connected, leaving
    // `app.session_id` = None when the SdkMessage arrives.
    emit_connected(
        bridge.event_tx(),
        &client,
        &session_id,
        &cwd_owned,
        launch_settings,
        resume_id,
        &config_dir,
        display_name.as_deref(),
    )
    .await;

    // Reader subtask - owns the events receiver. Client is the writer-side
    // handle (Arc-backed, Clone) and stays on the bridge; the reader also
    // keeps a clone so session-id-less frames (Error, RateLimitEvent) can
    // be stamped with the LIVE session id, not the frozen-at-spawn one.
    let reader_event_tx = bridge.event_tx().clone();
    let reader_session_id = session_id.clone();
    let reader_client = client.clone();
    let span = tracing::info_span!("sdk_reader", session_id = %reader_session_id);
    tokio::spawn(
        reader_loop(events, reader_event_tx, reader_session_id, reader_client, sdk_server_names)
            .instrument(span),
    );
    Ok(())
}

/// The Connected payload's mode state: the launch-time mode read from
/// the init record, falling back to the launch settings' stamped
/// defaultMode, with bypass offered only when the session launched
/// into it.
fn initial_mode_state(
    init_record: Option<&serde_json::Map<String, serde_json::Value>>,
    launch_settings_record: Option<&serde_json::Map<String, serde_json::Value>>,
    supports_auto_mode: bool,
) -> forge_primitives::ModeState {
    let raw_permission_mode = init_record
        .and_then(|r| r.get("permissionMode"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            launch_settings_record
                .and_then(|r| r.get(crate::client::SessionLaunchSettings::PERMISSIONS_KEY))
                .and_then(serde_json::Value::as_object)
                .and_then(|p| {
                    p.get(crate::client::SessionLaunchSettings::PERMISSIONS_DEFAULT_MODE_KEY)
                })
                .and_then(serde_json::Value::as_str)
        });
    let mode =
        raw_permission_mode.and_then(PermissionMode::from_wire).unwrap_or(PermissionMode::Ask);
    let launched_into_bypass = mode == PermissionMode::BypassPermissions;
    let supported = bridge_commands::supported_mode_ids_filtered(
        supports_auto_mode,
        launched_into_bypass,
        Some(mode),
        &[],
    );
    bridge_commands::build_mode_state_from_supported(mode, &supported)
}

/// Build the typed `Connected` envelope from the SDK's cached init data
/// + the initialize `control_response`, and emit it onto `event_tx`.
async fn emit_connected(
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
    client: &Client,
    session_id: &str,
    cwd: &str,
    launch_settings: &crate::client::SessionLaunchSettings,
    resume_id: Option<&str>,
    config_dir: &Path,
    display_name: Option<&str>,
) {
    let server_info = client.get_server_info().cloned();
    let init_data = client.initial_session_data().cloned();

    let available_models =
        session_lifecycle::map_available_models(server_info.as_ref().and_then(|v| v.get("models")));
    let init_record = init_data.as_ref().and_then(serde_json::Value::as_object);

    // The CLI doesn't emit `system/init` until both the initialize
    // control_response AND a user message have landed. Fall back to
    // launch_settings (settings.json) for the initial Connected.
    let launch_settings_record =
        launch_settings.settings.as_ref().and_then(serde_json::Value::as_object);
    let init_model_id = init_record
        .and_then(|r| r.get("model"))
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            launch_settings_record.and_then(|r| r.get("model")).and_then(serde_json::Value::as_str)
        })
        .unwrap_or("")
        .to_owned();
    let current_model = session_lifecycle::resolve_current_model_from_inputs(
        &init_model_id,
        None,
        None,
        &available_models,
    );
    let mode = Some(initial_mode_state(
        init_record,
        launch_settings_record,
        current_model.supports_auto_mode == Some(true),
    ));

    let resumed = resume_id
        .map(|prev_session_id| load_history_messages(config_dir, prev_session_id, cwd, session_id));
    let compaction_count = resumed.as_ref().map_or(0, |r| r.compaction_count);
    let history_updates = resumed.map(|r| r.messages).filter(|messages| !messages.is_empty());

    if event_tx
        .send(AgentEvent::Connected {
            session_id: session_id.to_owned(),
            cwd: cwd.to_owned(),
            current_model,
            available_models,
            mode,
            history_updates,
            compaction_count,
        })
        .is_err()
    {
        tracing::warn!(
            target: crate::logging::targets::BRIDGE_LIFECYCLE,
            session_id,
            "Connected event channel closed before emit; session stuck on Connecting"
        );
    }

    // account_info_from_shell shells out to `claude auth status` (~50ms
    // blocking per the docstring) - wrap in spawn_blocking so the
    // async worker doesn't park a tokio worker thread for the
    // duration. account_info_from_init is in-memory, no I/O.
    let account = if let Some(account) = client.account_info_from_init() {
        Some(account)
    } else {
        let config_dir_owned = config_dir.to_owned();
        match tokio::task::spawn_blocking(move || {
            crate::cloud::auth_status::account_info_from_shell(&config_dir_owned)
        })
        .await
        {
            Ok(opt) => opt,
            Err(join_err) => {
                tracing::warn!(
                    target: crate::logging::targets::BRIDGE_LIFECYCLE,
                    error = %join_err,
                    "account_info_from_shell spawn_blocking task panicked"
                );
                None
            }
        }
    };
    if let Some(account) = account {
        let forge_account =
            display_name.map(|d| forge_primitives::ForgeAccountIdentity::new(d.to_owned()));
        if event_tx
            .send(AgentEvent::StatusSnapshot {
                session_id: session_id.to_owned(),
                account,
                forge_account,
            })
            .is_err()
        {
            tracing::warn!(
                target: crate::logging::targets::BRIDGE_LIFECYCLE,
                session_id,
                "StatusSnapshot event channel closed before emit"
            );
        }
    }

    if event_tx
        .send(AgentEvent::SessionsListed { sessions: list_recent_sessions(config_dir, cwd).await })
        .is_err()
    {
        tracing::warn!(
            target: crate::logging::targets::BRIDGE_LIFECYCLE,
            session_id,
            "SessionsListed event channel closed before emit"
        );
    }
}

/// Replayable history for a resumed session plus the compaction count
/// its transcript records - the only place that count survives a resume.
struct ResumedHistory {
    messages: Vec<forge_primitives::Message>,
    compaction_count: u32,
}

fn load_history_messages(
    config_dir: &Path,
    prev_session_id: &str,
    cwd: &str,
    session_id: &str,
) -> ResumedHistory {
    let dir = if cwd.is_empty() { None } else { Some(cwd) };
    let history =
        crate::userdata::catalog::scan::get_session_messages(config_dir, prev_session_id, dir);
    let compaction_count = history.compaction_count;
    let raw: Vec<serde_json::Value> = history
        .messages
        .into_iter()
        .map(|m| {
            let kind = match m.kind {
                forge_primitives::SessionMessageKind::User => "user",
                forge_primitives::SessionMessageKind::Assistant => "assistant",
            };
            serde_json::json!({
                "type": kind,
                "message": m.message,
                "parent_tool_use_id": m.parent_tool_use_id,
            })
        })
        .collect();
    let mut synthesized = crate::replay::synthesize_replay_messages(&raw);
    // Stamp the resumed session_id on every synthesised Message - the
    // synthesizer leaves it empty so the caller picks the right value.
    for msg in &mut synthesized {
        match msg {
            forge_primitives::Message::Assistant { session_id: s, .. }
            | forge_primitives::Message::User { session_id: s, .. } => {
                session_id.clone_into(s);
            }
            _ => {}
        }
    }
    ResumedHistory { messages: synthesized, compaction_count }
}

async fn list_recent_sessions(
    config_dir: &Path,
    cwd: &str,
) -> Vec<forge_primitives::SessionListEntry> {
    use forge_primitives::SessionListEntry;
    const MAX_RECENT: usize = 50;
    let dir = if cwd.is_empty() { None } else { Some(cwd) };
    // One project's recent sessions, off the boot path, and this crate
    // holds no store to cache into.
    crate::userdata::catalog::scan::list_sessions(config_dir, dir, Some(MAX_RECENT), 0, false, None)
        .await
        .into_iter()
        .map(|info| SessionListEntry {
            session_id: info.session_id,
            summary: info.summary,
            last_modified_ms: info.last_modified,
            cwd: info.cwd,
            custom_title: info.custom_title,
            first_prompt: info.first_prompt,
        })
        .collect()
}

/// Resolve the session id to stamp on an inbound SDK frame. Frames that
/// carry their own id use it verbatim; session-id-less frames (Error,
/// RateLimitEvent) inherit the client's LIVE id, falling back to the
/// spawn-time id only while the live one is still empty (before
/// `system/init` binds the real UUID). Without the live preference a
/// mid-session fatal Error on a fresh session gets stamped "" and the
/// TUI drops it as an unknown session.
fn frame_session_id(
    msg_session_id: Option<&str>,
    live_session_id: &str,
    spawn_session_id: &str,
) -> String {
    match msg_session_id {
        Some(sid) => sid.to_owned(),
        None if !live_session_id.is_empty() => live_session_id.to_owned(),
        None => spawn_session_id.to_owned(),
    }
}

async fn reader_loop(
    mut events: forge_sdk::ClientEvents,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    spawn_session_id: String,
    client: Client,
    sdk_server_names: Vec<String>,
) {
    let mut heal_evaluated = false;
    while let Some(item) = events.recv().await {
        match item {
            Ok(msg) => {
                let session_id_for_sdk_msg =
                    frame_session_id(msg.session_id(), &client.session_id(), &spawn_session_id);
                log_failed_mcp_servers(&msg, &session_id_for_sdk_msg);
                let fire = heal_due(&msg, &mut heal_evaluated, &sdk_server_names);
                if event_tx
                    .send(AgentEvent::SdkMessage {
                        session_id: session_id_for_sdk_msg.clone(),
                        msg,
                    })
                    .is_err()
                {
                    return;
                }
                if fire {
                    let client = client.clone();
                    let names = sdk_server_names.clone();
                    let session_id = session_id_for_sdk_msg;
                    // Detached: the CLI answers mcp_set_servers only
                    // after the re-handshake it triggers completes, so
                    // awaiting inline would freeze this event pump for
                    // the whole re-handshake.
                    tokio::spawn(async move {
                        readd_sdk_mcp_servers(&client, &names, &session_id).await;
                    });
                }
            }
            Err(err) => {
                // A self-initiated teardown also tears the pipe down;
                // that is not a death and must not surface as one.
                if client.disconnect_requested() {
                    tracing::info!(
                        target: crate::logging::targets::BRIDGE_LIFECYCLE,
                        error = %err,
                        "forge_sdk reader: stream errored after a requested disconnect",
                    );
                    return;
                }
                tracing::error!(
                    target: crate::logging::targets::BRIDGE_LIFECYCLE,
                    error = %err,
                    "forge_sdk reader: events stream errored",
                );
                let _ = event_tx.send(AgentEvent::ConnectionFailed {
                    message: format!("the claude subprocess stream errored: {err}"),
                });
                return;
            }
        }
    }
    // The session swap and teardown paths both call `Client::disconnect`
    // before dropping their client; the stream end that follows is the
    // expected consequence, not a death. Only an UNrequested end (the
    // child exiting on its own) is a ConnectionFailed.
    if client.disconnect_requested() {
        tracing::info!(
            target: crate::logging::targets::BRIDGE_LIFECYCLE,
            "forge_sdk reader: stream closed after a requested disconnect",
        );
        return;
    }
    tracing::info!(
        target: crate::logging::targets::BRIDGE_LIFECYCLE,
        "forge_sdk reader: events stream closed",
    );
    let _ = event_tx.send(AgentEvent::ConnectionFailed {
        message: "the claude subprocess closed its output stream".to_owned(),
    });
}

/// The `data` payload of a `system/init` frame, else `None`.
fn init_data_of(msg: &forge_primitives::Message) -> Option<&serde_json::Value> {
    let forge_primitives::Message::System { subtype, data, .. } = msg else {
        return None;
    };
    (subtype == "init").then_some(data)
}

/// Whether the frame just received should fire the boot-tool heal. Pure
/// so the frame gate, the latch, and the predicate are testable without
/// a client. Only the boot `system/init` frame can fire it - the latch
/// advances on the first init frame seen either way - so a mid-session
/// init that lacks the tools (a user toggled the server off) is never
/// "healed" back on. The heal's OUTCOME is not checked here: init is
/// emitted per user message, so a "next frame" may never come or may
/// land inside the heal window; the re-add response itself carries the
/// outcome (see `readd_sdk_mcp_servers`).
///
/// Detection depends on the boot init frame reaching this loop:
/// `Client::spawn` swallows any pre-initialize init into its cached
/// init data, and today the CLI emits init only after a user message,
/// so the boot frame always lands here - if that seam changes, the
/// predicate silently never runs.
fn heal_due(
    msg: &forge_primitives::Message,
    boot_evaluated: &mut bool,
    server_names: &[String],
) -> bool {
    let Some(data) = init_data_of(msg) else { return false };
    if *boot_evaluated {
        return false;
    }
    *boot_evaluated = true;
    sdk_servers_missing_tools(data, server_names)
}

/// True when any registered sdk MCP server contributed no tools to the
/// session's init frame. The CLI serves an sdk server's handshake over
/// the control channel with a bounded request budget; when boot stalls
/// outlive it, the handshake is abandoned and the CLI then reports the
/// server `connected` with zero tools and never re-runs the handshake
/// itself.
fn sdk_servers_missing_tools(init_data: &serde_json::Value, server_names: &[String]) -> bool {
    let tools = init_data.get("tools").and_then(serde_json::Value::as_array);
    server_names.iter().any(|name| {
        let prefix = format!("mcp__{name}__");
        !tools
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .any(|tool| tool.starts_with(&prefix))
    })
}

/// The CLI answers `mcp_set_servers` only after the re-handshake it
/// triggers completes, so the wait is bounded past the CLI's own ~30s
/// request budget.
const SDK_MCP_READD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Force the CLI to re-handshake every registered sdk MCP server by
/// removing them from its dynamic set and adding them back. The CLI
/// sends the re-add response only after the re-handshake completes, so
/// the response is the outcome: `errors` empty plus a non-empty `added`
/// means the tools are registered again. Every outcome logs here - a
/// "next init frame" is no verification signal, since init is emitted
/// per user message.
async fn readd_sdk_mcp_servers(client: &Client, server_names: &[String], session_id: &str) {
    if let Err(err) =
        tokio::time::timeout(SDK_MCP_READD_TIMEOUT, client.mcp_set_servers(serde_json::json!({})))
            .await
    {
        // The removal never landed, so the CLI-side dynamic set is
        // untouched and the boot-time registration stands.
        tracing::warn!(
            target: crate::logging::targets::BRIDGE_LIFECYCLE,
            session_id = %session_id,
            error = %err,
            "sdk mcp tool heal: removal did not answer; boot-time registration kept"
        );
        return;
    }
    let servers: serde_json::Map<String, serde_json::Value> = server_names
        .iter()
        .map(|name| (name.clone(), serde_json::json!({ "type": "sdk", "name": name })))
        .collect();
    let response = match tokio::time::timeout(
        SDK_MCP_READD_TIMEOUT,
        client.mcp_set_servers(serde_json::json!(servers)),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(err)) => {
            tracing::error!(
                target: crate::logging::targets::BRIDGE_LIFECYCLE,
                session_id = %session_id,
                error = %err,
                "sdk mcp tool heal: re-add failed after a landed removal; \
                 the sdk servers are deregistered - restart the session"
            );
            return;
        }
        Err(_) => {
            tracing::error!(
                target: crate::logging::targets::BRIDGE_LIFECYCLE,
                session_id = %session_id,
                "sdk mcp tool heal: re-add timed out after a landed removal; \
                 the sdk servers are deregistered - restart the session"
            );
            return;
        }
    };
    for (name, message) in &response.errors {
        tracing::warn!(
            target: crate::logging::targets::BRIDGE_LIFECYCLE,
            session_id = %session_id,
            server = %name,
            error = %message,
            "sdk mcp tool heal: server failed to re-handshake on re-add"
        );
    }
    if response.added.is_empty() {
        tracing::warn!(
            target: crate::logging::targets::BRIDGE_LIFECYCLE,
            session_id = %session_id,
            removed = ?response.removed,
            "sdk mcp tool heal: re-add reported nothing added; tools not restored"
        );
        return;
    }
    tracing::info!(
        target: crate::logging::targets::BRIDGE_LIFECYCLE,
        session_id = %session_id,
        added = ?response.added,
        "sdk mcp tool heal: tools re-registered, outcome verified from the re-add response"
    );
}

/// Record any MCP server the init handshake reports as `failed`.
///
/// The handshake is the only place this surfaces: no status-change
/// event follows on the wire, so a server that failed to bind is
/// otherwise just absent, with nothing written anywhere. Reads the wire
/// strings rather than the typed status enum, so a status the enum does
/// not know yet cannot cost the whole list.
fn log_failed_mcp_servers(msg: &forge_primitives::Message, session_id: &str) {
    let forge_primitives::Message::System { subtype, data, .. } = msg else {
        return;
    };
    if subtype != "init" {
        return;
    }
    let failed = data
        .get("mcp_servers")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|server| server.get("status").and_then(serde_json::Value::as_str) == Some("failed"))
        .filter_map(|server| server.get("name").and_then(serde_json::Value::as_str));
    for name in failed {
        tracing::warn!(
            target: crate::logging::targets::BRIDGE_LIFECYCLE,
            event_name = "mcp_server_failed",
            message = "MCP server failed to connect; it is absent from this session",
            outcome = "failed",
            session_id = %session_id,
            server = %name,
        );
    }
}

pub(crate) async fn send_prompt(
    client: &Client,
    chunks: Vec<forge_primitives::PromptChunk>,
) -> anyhow::Result<()> {
    debug_assert!(!chunks.is_empty(), "send_prompt called with empty chunks");
    if chunks.iter().all(|c| c.kind == "text") {
        let prompt: String =
            chunks.iter().filter_map(|c| c.value.as_str()).collect::<Vec<_>>().join("\n");
        client.send_user_message(&prompt).await?;
    } else {
        let content: Vec<serde_json::Value> = chunks
            .into_iter()
            .map(|c| match c.kind.as_str() {
                "text" => serde_json::json!({
                    "type": "text",
                    "text": c.value.as_str().unwrap_or(""),
                }),
                "image" => serde_json::json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": c.value.get("mime_type").and_then(|v| v.as_str()).unwrap_or("image/png"),
                        "data": c.value.get("data").and_then(|v| v.as_str()).unwrap_or(""),
                    },
                }),
                _ => c.value,
            })
            .collect();
        client.send_user_message_with_content(&content).await?;
    }
    Ok(())
}

pub(crate) fn parse_permission_mode(mode: &str) -> anyhow::Result<PermissionMode> {
    forge_primitives::permission::PermissionMode::from_wire(mode)
        .ok_or_else(|| anyhow::anyhow!("forge_sdk: unknown permission mode {mode:?}"))
}

// ----------------------------------------------------------------------------
// Permission / question round-trip
// ----------------------------------------------------------------------------

/// Default-off toggle for the CLI's `excludeDynamicSections` initialize
/// signal (the 2.1.204 `--exclude-dynamic-system-prompt-sections`
/// equivalent) that relocates cwd/env/memory-paths/git-status out of the
/// system prompt into the first user message for prompt-cache reuse.
/// Off maps to `None`, so the `initialize` body omits the key and the
/// wire stays byte-identical to today; flip to `true` to enable after
/// measuring.
const EXCLUDE_DYNAMIC_SECTIONS: bool = false;

/// Forge-stamped env keys a forge.toml env table can actually
/// override: `CLAUDE_CONFIG_DIR`, stamped into `options.env` before the
/// account-env loop. Reusing it silently overrides forge's stamp, so a
/// collision is warned - though the stamp still applies (forge.toml is
/// trusted, hand-authored). `CLAUDE_AGENT_SDK_VERSION` is deliberately
/// absent: process.rs stamps it LAST, unconditionally, after the
/// account-env loop, so forge always wins and a warn would be a false
/// alarm.
const FORGE_RESERVED_ENV_KEYS: &[&str] = &["CLAUDE_CONFIG_DIR"];

fn is_reserved_env_key(key: &str) -> bool {
    FORGE_RESERVED_ENV_KEYS.contains(&key)
}

/// Per-account spawn binding threaded into every `claude` subprocess:
/// the account's `config_dir` (exported as `CLAUDE_CONFIG_DIR`) and the
/// session's resolved env - `[env]` merged with `[accounts.env]` and the
/// spawning project's `[projects.<name>.env]`. Both come from the
/// bridge, distinct from the per-launch `SessionLaunchSettings`.
pub(crate) struct AccountBinding<'a> {
    pub config_dir: &'a Path,
    pub env: &'a HashMap<String, String>,
}

fn build_options_with_callback(
    cwd: &str,
    resume: Option<&str>,
    launch_settings: &crate::client::SessionLaunchSettings,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    pending: PendingResponses,
    pending_questions: PendingQuestions,
    session_id_slot: Arc<parking_lot::Mutex<String>>,
    extra_mcp_servers: Vec<(String, forge_sdk::mcp::McpServer)>,
    binding: &AccountBinding<'_>,
) -> Options {
    // A passthrough hook emits `AgentEvent::HookObservation` for every
    // PreToolUse input without altering the dispatch outcome, carrying
    // subagent attribution (`agent_id` + `agent_type`) - see #84.
    //
    // Observation must stay off `UserPromptSubmit`: the CLI answers a
    // callback timeout there with `suppressOriginalPrompt`, so a slow
    // control-stream reply discards the user's prompt.
    let pre_tool_observe_tx = event_tx.clone();
    let pre_tool_observe_sid = Arc::clone(&session_id_slot);

    let observation_hooks = HooksBuilder::new()
        .pre_tool_use("*", move |input: PreToolUseInput, _ctx: HookContext| {
            let tx = pre_tool_observe_tx.clone();
            let session_id = pre_tool_observe_sid.lock().clone();
            async move {
                let _ = tx.send(AgentEvent::HookObservation {
                    session_id,
                    tool_use_id: Some(input.tool_use_id.clone()),
                    permission_mode: input.base.permission_mode.clone(),
                    effort: input.base.effort.as_ref().map(|e| e.level.clone()),
                    agent_id: input.subagent.agent_id.clone(),
                    agent_type: input.subagent.agent_type.clone(),
                });
                HookDecision::passthrough()
            }
        })
        .build();

    let callback = move |ctx: ToolPermissionContext| {
        let event_tx = event_tx.clone();
        let pending = Arc::clone(&pending);
        let pending_questions = Arc::clone(&pending_questions);
        let session_id = session_id_slot.lock().clone();
        async move {
            if ctx.tool_name == bridge_user_interaction::ASK_USER_QUESTION_TOOL_NAME {
                run_ask_user_question(ctx, session_id, &event_tx, &pending_questions).await
            } else {
                run_permission_request(ctx, session_id, &event_tx, &pending).await
            }
        }
    };

    let mut b = OptionsBuilder::new()
        .can_use_tool(callback)
        .hooks(observation_hooks)
        .permission_prompt_tool_name("stdio");
    // Forge-workspace-supplied in-process MCP servers. Today the
    // only one is `forge` (peers tools on leads only); future
    // modules (worktree, memory) will hang under their own names.
    // Each spawned `claude` subprocess sees them
    // as `mcp__<server_name>__<tool_name>`.
    //
    // Derive the auto-approve predicate from the live server names
    // - anything matching a registered server prefix is opted-in
    // by definition (the user already configured the server in
    // forge.toml). forge-sdk's control_dispatch consults the
    // predicate before invoking can_use_tool, so no permission
    // prompt fires for these calls. Audit I6: keeps the prefix
    // knowledge with the configurator instead of hardcoded in
    // forge-sdk.
    let has_forge_mcp = extra_mcp_servers.iter().any(|(name, _)| name == "forge");
    let has_peer_tools = extra_mcp_servers
        .iter()
        .any(|(name, server)| name == "forge" && server.has_tool_prefix("peers__"));
    let auto_approve_prefixes: Vec<String> =
        extra_mcp_servers.iter().map(|(name, _)| format!("mcp__{name}__")).collect();
    if !auto_approve_prefixes.is_empty() {
        b = b.auto_approve_tool(move |tool_name: &str| {
            auto_approve_prefixes.iter().any(|p| tool_name.starts_with(p.as_str()))
        });
    }
    for (name, server) in extra_mcp_servers {
        b = b.mcp_server(name, server);
    }
    // When the `forge` MCP server is attached, inject a trust
    // instruction into the spawned session's system prompt so the
    // LLM treats peer-wrapped user-turns as user-authorized context
    // rather than untrusted prompt injection - and so it doesn't
    // ask the user for permission before invoking the peer tools.
    // Skips the inject entirely when no forge MCP is attached
    // (matches the "only when MCP is included" intent - keeps the
    // append text out of every other CLI spawn).
    //
    // Worker sessions append the LLM-supplied `charter` after the
    // trust prompt (one blank line between them). The charter
    // defines the worker's persona / goal; the trust prompt covers
    // the forge MCP coordination semantics that every forge session
    // needs.
    if has_forge_mcp {
        let append = build_forge_system_prompt(
            has_peer_tools,
            launch_settings.delegation_preamble.as_deref(),
            launch_settings.charter.as_deref(),
        );
        b = b.system_prompt(forge_sdk::SystemPromptKind::Preset {
            append: Some(append),
            exclude_dynamic_sections: EXCLUDE_DYNAMIC_SECTIONS.then_some(true),
        });
    }
    if !cwd.is_empty() {
        b = b.cwd(PathBuf::from(cwd));
    }
    if let Some(id) = resume {
        b = b.resume(id);
    }

    let mut applied_mode: Option<&'static str> = None;
    let mut applied_model: Option<String> = None;
    let mut applied_effort: Option<String> = None;
    let settings_present = launch_settings.settings.is_some();
    if let Some(settings_value) = launch_settings.settings.as_ref()
        && let Some(settings_record) = settings_value.as_object()
    {
        if let Some(perms) = settings_record
            .get(crate::client::SessionLaunchSettings::PERMISSIONS_KEY)
            .and_then(serde_json::Value::as_object)
            && let Some(default_mode_str) = perms
                .get(crate::client::SessionLaunchSettings::PERMISSIONS_DEFAULT_MODE_KEY)
                .and_then(serde_json::Value::as_str)
            && let Ok(mode) = parse_permission_mode(default_mode_str)
        {
            b = b.permission_mode(mode);
            applied_mode = Some(mode.as_cli_arg());
        }
        if let Some(model) = settings_record.get("model").and_then(serde_json::Value::as_str)
            && !model.trim().is_empty()
        {
            b = b.model(model);
            applied_model = Some(model.to_owned());
        }
        if let Some(effort) = settings_record.get("effortLevel").and_then(serde_json::Value::as_str)
            && !effort.trim().is_empty()
        {
            applied_effort = Some(effort.to_owned());
            b = b.extra_arg("effort", Some(effort.to_owned()));
        }
    }

    // Effort resolution order:
    //   1. settings.json `effortLevel` (handled above).
    //   2. `CLAUDE_CODE_EFFORT_LEVEL` env var - leave it to env
    //      inheritance so a user override isn't shadowed by `--effort`.
    //   3. Default to `--effort max`.
    let effort_source: &'static str = if applied_effort.is_some() {
        "settings"
    } else {
        match std::env::var("CLAUDE_CODE_EFFORT_LEVEL") {
            Ok(env_value) if !env_value.trim().is_empty() => {
                applied_effort = Some(env_value);
                "env_var"
            }
            _ => {
                applied_effort = Some("max".to_owned());
                b = b.extra_arg("effort", Some("max".to_owned()));
                "default_max"
            }
        }
    };

    // Spawn-path-specific extra CLI args (e.g. workers in git-repo
    // projects get `("worktree", Some(label))` so claude forks a
    // worktree at `<repo>/.claude/worktrees/<label>/`). The
    // OptionsBuilder's `extra_args` HashMap is last-write-wins on
    // insert; this loop runs after the function's own extra_arg calls
    // (`effort`), so any duplicate key in launch_settings overrides
    // what the function stamped.
    for (flag, value) in &launch_settings.extra_args {
        b = b.extra_arg(flag.clone(), value.clone());
    }

    // Swap tests drive the real spawn_session against forge-sdk's mock
    // fixture; production always runs the `claude` on PATH.
    #[cfg(test)]
    if let Some(path) = test_sdk_binary() {
        b = b.binary(path);
    }

    // Per-spawn `CLAUDE_CONFIG_DIR` - workspace-driven so each
    // `claude` subprocess reads/writes the bound account's
    // user-data tree (oauth tokens, projects history, settings).
    // Threaded through as a typed `Path` from the bridge.
    b = b.env("CLAUDE_CONFIG_DIR", binding.config_dir.to_string_lossy().to_string());

    // The session's effective env from forge.toml (hand-authored,
    // trusted) - `[env]`, then `[accounts.env]`, then the spawning
    // project's `[projects.<name>.env]`, narrowest winning - stamped
    // onto the child so an account or project can point `claude` at an
    // alternate endpoint or set any other env it needs. Runs after
    // the `CLAUDE_CONFIG_DIR` stamp so a caller could override it
    // deliberately; process.rs stamps `CLAUDE_AGENT_SDK_VERSION` last
    // regardless.
    for (key, value) in binding.env {
        if is_reserved_env_key(key) {
            tracing::warn!(
                target: crate::logging::targets::BRIDGE_LIFECYCLE,
                key = %key,
                "forge.toml [env] / [accounts.env] / [projects.<name>.env] sets a forge-reserved key; it overrides forge's own stamp",
            );
        }
        b = b.env(key, value);
    }

    tracing::info!(
        target: crate::logging::targets::BRIDGE_LIFECYCLE,
        event_name = "forge_sdk_options_built",
        message = "launch_settings → forge-sdk OptionsBuilder",
        outcome = "info",
        settings_present,
        applied_permission_mode = applied_mode.unwrap_or("(none)"),
        applied_model = applied_model.as_deref().unwrap_or("(none)"),
        applied_effort = applied_effort.as_deref().unwrap_or("(none)"),
        effort_source,
        cwd_present = !cwd.is_empty(),
        resume_present = resume.is_some(),
        config_dir = %binding.config_dir.display(),
    );
    b.build()
}

#[cfg(test)]
static TEST_SDK_BINARY: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

#[cfg(test)]
fn test_sdk_binary() -> Option<String> {
    TEST_SDK_BINARY.read().expect("test binary lock").clone()
}

#[cfg(test)]
pub(crate) fn set_test_sdk_binary(path: Option<String>) {
    *TEST_SDK_BINARY.write().expect("test binary lock") = path;
}

async fn run_permission_request(
    ctx: ToolPermissionContext,
    session_id: String,
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
    pending: &PendingResponses,
) -> PermissionDecision {
    let (tx, rx) = oneshot::channel();
    {
        let mut guard = pending.lock();
        // Guard against duplicate tool_use_id from upstream (CLI retry
        // bug, protocol drift). Without this, the second insert
        // silently drops the first oneshot and the original
        // `run_permission_request` hangs until the channel is dropped.
        if let Some(prev) = guard.insert(ctx.tool_use_id.clone(), tx) {
            tracing::warn!(
                target: crate::logging::targets::BRIDGE_LIFECYCLE,
                tool_use_id = %ctx.tool_use_id,
                "duplicate tool_use_id on permission request; cancelling prior oneshot"
            );
            // Best-effort resolve the displaced sender so the prior
            // run_permission_request future doesn't hang.
            let _ = prev.send(PermissionDecision::deny("displaced by duplicate tool_use_id"));
        }
    }
    let event = synth_permission_request(&session_id, &ctx);
    if event_tx.send(event).is_err() {
        return PermissionDecision::deny("event channel closed");
    }
    match rx.await {
        Ok(decision) => decision,
        Err(_) => PermissionDecision::deny("response channel closed"),
    }
}

async fn run_ask_user_question(
    ctx: ToolPermissionContext,
    session_id: String,
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
    pending_questions: &PendingQuestions,
) -> PermissionDecision {
    use forge_primitives::QuestionOutcome;

    let prompts = bridge_user_interaction::parse_ask_user_question_prompts(&ctx.tool_input);
    if prompts.is_empty() {
        return PermissionDecision::allow();
    }

    let total = prompts.len() as u64;
    let base_tool_call = synth_question_base_tool_call(&ctx);
    let mut answers = serde_json::Map::new();
    let mut annotations = serde_json::Map::new();

    for (index, prompt) in prompts.iter().enumerate() {
        let request = bridge_user_interaction::build_question_request(
            &base_tool_call,
            prompt,
            index as u64,
            total,
        );
        let (tx, rx) = oneshot::channel();
        {
            let mut guard = pending_questions.lock();
            if let Some(prev) = guard.insert(ctx.tool_use_id.clone(), tx) {
                tracing::warn!(
                    target: crate::logging::targets::BRIDGE_LIFECYCLE,
                    tool_use_id = %ctx.tool_use_id,
                    "duplicate tool_use_id on question request; cancelling prior oneshot"
                );
                let _ = prev.send(QuestionOutcome::Cancelled);
            }
        }
        if event_tx
            .send(AgentEvent::QuestionRequest {
                session_id: session_id.clone(),
                request: request.clone(),
            })
            .is_err()
        {
            return PermissionDecision::deny("event channel closed");
        }
        let Ok(outcome) = rx.await else {
            return PermissionDecision::deny("response channel closed");
        };
        match outcome {
            QuestionOutcome::Answered { selected_option_ids, annotation } => {
                let selected: Vec<forge_primitives::QuestionOption> = request
                    .prompt
                    .options
                    .iter()
                    .filter(|opt| selected_option_ids.iter().any(|id| id == &opt.option_id))
                    .cloned()
                    .collect();
                let notes_provided = annotation
                    .as_ref()
                    .and_then(|a| a.notes.as_deref())
                    .is_some_and(|n| !n.trim().is_empty());
                if selected.is_empty() {
                    // "Tell Claude something else" path: no canonical
                    // option matched, but if the user supplied notes the
                    // answer is "Other" with the free-text in
                    // annotations (matches Anthropic's schema where
                    // Other is the always-available custom-text option).
                    if notes_provided {
                        answers.insert(
                            prompt.question_key.clone(),
                            serde_json::Value::String("Other".to_owned()),
                        );
                        if let Some(ann) = annotation {
                            match serde_json::to_value(&ann) {
                                Ok(v) => {
                                    annotations.insert(prompt.question_key.clone(), v);
                                }
                                Err(err) => {
                                    tracing::warn!(
                                        target: "forge_agent::forge_sdk_worker",
                                        question = %prompt.question,
                                        error = %err,
                                        "failed to serialise question annotation; dropping"
                                    );
                                }
                            }
                        }
                        continue;
                    }
                    return PermissionDecision::deny("Question answer was invalid");
                }
                if !prompt.multi_select && selected.len() != 1 {
                    return PermissionDecision::deny("Question answer was invalid");
                }
                let answer =
                    selected.iter().map(|o| o.label.as_str()).collect::<Vec<_>>().join(", ");
                answers.insert(prompt.question_key.clone(), serde_json::Value::String(answer));
                if let Some(annotation) =
                    bridge_user_interaction::derive_annotation(&selected, annotation.as_ref())
                {
                    match serde_json::to_value(&annotation) {
                        Ok(v) => {
                            annotations.insert(prompt.question_key.clone(), v);
                        }
                        Err(err) => {
                            tracing::warn!(
                                target: "forge_agent::forge_sdk_worker",
                                question = %prompt.question,
                                error = %err,
                                "failed to serialise question annotation; dropping"
                            );
                        }
                    }
                }
            }
            QuestionOutcome::Cancelled => {
                return PermissionDecision::deny("Question cancelled");
            }
        }
    }

    let updated_input =
        bridge_user_interaction::build_updated_input(&ctx.tool_input, answers, annotations);
    tracing::debug!(
        target: "forge_agent::forge_sdk_worker",
        tool_use_id = %ctx.tool_use_id,
        answer_keys = ?updated_input.get("answers").and_then(serde_json::Value::as_object)
            .map(|a| a.keys().collect::<Vec<_>>()),
        "ask_user_question answered",
    );
    PermissionDecision::allow_with_input(updated_input)
}

fn synth_question_base_tool_call(ctx: &ToolPermissionContext) -> forge_primitives::ToolCall {
    use forge_primitives::{ToolCall, ToolCallStatus, ToolKind};
    ToolCall {
        tool_call_id: ctx.tool_use_id.clone(),
        title: bridge_user_interaction::ASK_USER_QUESTION_TOOL_NAME.to_owned(),
        kind: ToolKind::Other,
        status: ToolCallStatus::Pending,
        content: Vec::new(),
        raw_input: Some(ctx.tool_input.clone()),
        raw_output: None,
        output_metadata: None,
        task_metadata: None,
        locations: Vec::new(),
        meta: None,
    }
}

pub(crate) fn deliver_permission_response(
    pending: &PendingResponses,
    tool_call_id: &str,
    outcome: forge_primitives::PermissionOutcome,
) {
    let Some(tx) = pending.lock().remove(tool_call_id) else {
        tracing::warn!(
            target: crate::logging::targets::APP_PERMISSION,
            tool_call_id,
            "forge_sdk: PermissionResponse for unknown tool_call_id (already drained?)",
        );
        return;
    };
    let decision = match outcome {
        forge_primitives::PermissionOutcome::Selected {
            action, notes_text, edited_input, ..
        } => dispatch_permission_action(action, notes_text.as_deref().unwrap_or(""), edited_input),
        forge_primitives::PermissionOutcome::Cancelled => {
            PermissionDecision::deny("user cancelled")
        }
    };
    if tx.send(decision).is_err() {
        tracing::debug!(
            target: crate::logging::targets::APP_PERMISSION,
            tool_call_id,
            "PermissionResponse oneshot receiver dropped before delivery",
        );
    }
}

/// Build the `PermissionDecision` for a submitted option's `action`.
/// `notes_text` is the user's "tell Claude" feedback string (or
/// empty); consumed only when the action is `Deny`. `edited_input` is
/// the user's modified tool args; consumed only when the action is
/// `AllowWithInput`.
pub(crate) fn dispatch_permission_action(
    action: forge_primitives::permission_ui::PermissionAction,
    notes_text: &str,
    edited_input: Option<serde_json::Value>,
) -> PermissionDecision {
    use forge_primitives::permission_ui::PermissionAction;
    match action {
        PermissionAction::Allow => PermissionDecision::allow(),
        PermissionAction::AllowWithUpdates { updates } => {
            PermissionDecision::allow().with_updated_permissions(updates)
        }
        PermissionAction::AllowWithInput => {
            PermissionDecision::allow_with_input(edited_input.unwrap_or_default())
        }
        PermissionAction::Deny => {
            let reason = if notes_text.trim().is_empty() {
                "Denied by user".to_owned()
            } else {
                notes_text.trim().to_owned()
            };
            PermissionDecision::deny(reason)
        }
    }
}

pub(crate) fn deliver_question_response(
    pending: &PendingQuestions,
    tool_call_id: &str,
    outcome: forge_primitives::QuestionOutcome,
) {
    let Some(tx) = pending.lock().remove(tool_call_id) else {
        tracing::warn!(
            target: crate::logging::targets::APP_PERMISSION,
            tool_call_id,
            "forge_sdk: QuestionResponse for unknown tool_call_id",
        );
        return;
    };
    if tx.send(outcome).is_err() {
        tracing::debug!(
            target: crate::logging::targets::APP_PERMISSION,
            tool_call_id,
            "QuestionResponse oneshot receiver dropped before delivery",
        );
    }
}

fn synth_permission_request(session_id: &str, ctx: &ToolPermissionContext) -> AgentEvent {
    use forge_primitives::{
        PermissionDisplay, PermissionRequest, ToolCall, ToolCallStatus, ToolKind,
    };
    let tool_call = ToolCall {
        tool_call_id: ctx.tool_use_id.clone(),
        title: ctx.tool_name.clone(),
        kind: ToolKind::Execute,
        status: ToolCallStatus::Pending,
        content: Vec::new(),
        raw_input: Some(ctx.tool_input.clone()),
        raw_output: None,
        output_metadata: None,
        task_metadata: None,
        locations: Vec::new(),
        meta: None,
    };
    let display = PermissionDisplay {
        title: ctx.title.clone(),
        display_name: ctx.display_name.clone(),
        description: ctx.description.clone(),
        decision_reason: ctx.decision_reason.clone(),
    };
    AgentEvent::PermissionRequest {
        session_id: session_id.to_owned(),
        request: PermissionRequest {
            tool_call,
            options: build_permission_options(ctx),
            display: Some(display),
        },
    }
}

/// Construct the contextual option list for a permission prompt.
/// Derives "Allow always for X" / "Allow always & add dirs" / "Allow
/// always & switch mode" entries from `ctx.suggestions`; adds the
/// synthesized "Allow with edits" for editable tools and the universal
/// "Tell Claude something else" escape hatch.
///
/// Wire reality (captured 2026-05-18; see baselines/sdk/2.1.117/):
/// - `Read` outside workspace -> `addRules` with `{toolName, ruleContent}`.
///   macOS sends BOTH `//tmp/**` AND `//private/tmp/**` -- these collapse
///   into ONE display option whose action carries BOTH rules.
/// - `Write` / `Edit` outside workspace -> `setMode -> acceptEdits` +
///   `addDirectories` (NOT addRules). Different variant mix.
/// - `ExitPlanMode` -> `suggestions: []` (empty). Plain Permission.
fn build_permission_options(
    ctx: &ToolPermissionContext,
) -> Vec<forge_primitives::PermissionOption> {
    use forge_primitives::PermissionOption;
    use forge_primitives::permission_ui::{PermissionAction, PermissionOptionKind};
    use forge_primitives::permissions::PermissionUpdate;

    let mut opts: Vec<PermissionOption> = Vec::new();

    // 1. Allow once is universal.
    opts.push(PermissionOption {
        option_id: "allow_once".to_owned(),
        name: "Allow once".to_owned(),
        kind: PermissionOptionKind::Allow,
        action: PermissionAction::Allow,
        recommended: false,
    });

    // 2. Derive "Allow always" options from ctx.suggestions, with macOS
    //    /tmp <-> /private/tmp dedupe for AddRules.
    let canonical = canonicalize_suggestions(&ctx.suggestions);
    for (i, update) in canonical.iter().enumerate() {
        let (name, action) = match update {
            PermissionUpdate::AddRules { rules, behavior, .. } => {
                if *behavior != forge_primitives::permissions::PermissionBehavior::Allow {
                    continue;
                }
                let summary = summarize_rules(rules);
                (
                    format!("Allow always for {} \u{b7} {}", ctx.tool_name, summary),
                    PermissionAction::AllowWithUpdates { updates: vec![update.clone()] },
                )
            }
            PermissionUpdate::AddDirectories { directories, .. } => {
                let summary = summarize_dirs(directories);
                (
                    format!("Allow always & add {summary} to allowed dirs"),
                    PermissionAction::AllowWithUpdates { updates: vec![update.clone()] },
                )
            }
            PermissionUpdate::SetMode { mode, destination } => {
                // Forge promotes claude's `acceptEdits` suggestion to
                // `auto` (bypassPermissions) - the user has a global
                // Auto/Ask toggle, so the partial-auto stepping stone
                // is confusing. Other SetMode targets pass through.
                let target_mode =
                    if matches!(mode, forge_primitives::permission::PermissionMode::AcceptEdits) {
                        forge_primitives::permission::PermissionMode::Auto
                    } else {
                        *mode
                    };
                let swapped_update =
                    PermissionUpdate::SetMode { mode: target_mode, destination: *destination };
                (
                    format!("Allow always & switch to {} mode", target_mode.display_name()),
                    PermissionAction::AllowWithUpdates { updates: vec![swapped_update] },
                )
            }
            // RemoveRules / ReplaceRules / RemoveDirectories aren't
            // user-facing prompt options. Skip.
            PermissionUpdate::RemoveRules { .. }
            | PermissionUpdate::ReplaceRules { .. }
            | PermissionUpdate::RemoveDirectories { .. } => continue,
        };
        opts.push(PermissionOption {
            option_id: format!("allow_always_{i}"),
            name,
            kind: PermissionOptionKind::Allow,
            action,
            recommended: false,
        });
    }

    // 3. "Allow with edits" for editable tools.
    if is_editable_tool(&ctx.tool_name) {
        opts.push(PermissionOption {
            option_id: "allow_with_edits".to_owned(),
            name: "Allow with edits".to_owned(),
            kind: PermissionOptionKind::Edit,
            action: PermissionAction::AllowWithInput,
            recommended: false,
        });
    }

    // 4. Deny.
    opts.push(PermissionOption {
        option_id: "deny".to_owned(),
        name: "Deny".to_owned(),
        kind: PermissionOptionKind::Deny,
        action: PermissionAction::Deny,
        recommended: false,
    });

    // 5. Universal: Tell Claude something else.
    opts.push(PermissionOption {
        option_id: "tell_claude".to_owned(),
        name: "Tell Claude something else".to_owned(),
        kind: PermissionOptionKind::Notes,
        action: PermissionAction::Deny,
        recommended: false,
    });

    opts
}

/// Walk the suggestions list and collapse macOS `/tmp` <-> `/private/tmp`
/// AddRules mirrors into one merged entry. Two AddRules suggestions with
/// the same toolName + behavior + destination, whose rule_content differs
/// only by the `/tmp/` <-> `/private/tmp/` prefix, are merged into a
/// single AddRules whose `rules` vec contains BOTH entries so the CLI
/// installs both rules even though the UI shows one option.
fn canonicalize_suggestions(
    suggestions: &[forge_primitives::permissions::PermissionUpdate],
) -> Vec<forge_primitives::permissions::PermissionUpdate> {
    use forge_primitives::permissions::PermissionUpdate;

    let mut result: Vec<PermissionUpdate> = Vec::with_capacity(suggestions.len());
    let mut skip: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();

    for (i, update) in suggestions.iter().enumerate() {
        if skip.contains(&i) {
            continue;
        }
        let PermissionUpdate::AddRules { rules: rules_a, behavior: beh_a, destination: dst_a } =
            update
        else {
            result.push(update.clone());
            continue;
        };
        let mut merged_rules = rules_a.clone();
        for (j, other) in suggestions.iter().enumerate().skip(i + 1) {
            if skip.contains(&j) {
                continue;
            }
            let PermissionUpdate::AddRules { rules: rules_b, behavior: beh_b, destination: dst_b } =
                other
            else {
                continue;
            };
            if beh_a != beh_b || dst_a != dst_b {
                continue;
            }
            if is_tmp_mirror_pair(rules_a, rules_b) {
                merged_rules.extend(rules_b.iter().cloned());
                skip.insert(j);
            }
        }
        result.push(PermissionUpdate::AddRules {
            rules: merged_rules,
            behavior: *beh_a,
            destination: *dst_a,
        });
    }
    result
}

/// True if `rules_a` and `rules_b` differ only by the macOS
/// `/tmp/` <-> `/private/tmp/` prefix in their rule_content strings.
fn is_tmp_mirror_pair(
    rules_a: &[forge_primitives::permissions::PermissionRuleValue],
    rules_b: &[forge_primitives::permissions::PermissionRuleValue],
) -> bool {
    if rules_a.len() != rules_b.len() {
        return false;
    }
    rules_a.iter().zip(rules_b.iter()).all(|(a, b)| {
        if a.tool_name != b.tool_name {
            return false;
        }
        match (a.rule_content.as_deref(), b.rule_content.as_deref()) {
            (Some(ca), Some(cb)) => {
                let na = normalize_tmp_path(ca);
                let nb = normalize_tmp_path(cb);
                na == nb && ca != cb
            }
            // No content on either side: not a path-prefix mirror.
            _ => false,
        }
    })
}

/// Strip the `/private` prefix from `//private/tmp/...` rule_content so
/// the macOS mirror normalizes to the canonical `/tmp/...` form.
fn normalize_tmp_path(content: &str) -> String {
    content.replacen("//private/tmp/", "//tmp/", 1)
}

fn summarize_rules(rules: &[forge_primitives::permissions::PermissionRuleValue]) -> String {
    // Display: collapse the macOS /tmp + /private/tmp pair to "/tmp/**".
    let mut shown: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for r in rules {
        let label = r.rule_content.as_deref().map_or_else(
            || format!("any {} invocation", r.tool_name),
            |c| format!("paths matching {}", normalize_tmp_path(c)),
        );
        shown.insert(label);
    }
    shown.into_iter().collect::<Vec<_>>().join(", ")
}

fn summarize_dirs(dirs: &[String]) -> String {
    // Display: collapse the macOS /tmp + /private/tmp pair.
    let mut shown: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for d in dirs {
        shown.insert(d.replacen("/private/tmp", "/tmp", 1));
    }
    shown.into_iter().collect::<Vec<_>>().join(", ")
}

/// Tools whose `tool_input` is meaningful to edit before approving.
fn is_editable_tool(tool_name: &str) -> bool {
    matches!(tool_name, "Bash" | "Edit" | "Write" | "MultiEdit" | "NotebookEdit")
}

pub(crate) fn clamp_percentage_to_u8(p: f64) -> u8 {
    if p.is_nan() {
        return 0;
    }
    // Clamped to 0..=100 first, so neither truncation nor sign loss can fire.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let n = p.clamp(0.0, 100.0).round() as u8;
    n
}

#[cfg(test)]
mod tests {
    use super::{
        PendingQuestions, PendingResponses, build_forge_system_prompt, deliver_permission_response,
        deliver_question_response, frame_session_id, initial_mode_state, log_failed_mcp_servers,
        run_ask_user_question, synth_permission_request,
    };

    /// Buffer tracing output so an emitted record can be read back.
    #[derive(Clone, Default)]
    struct LogCapture(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture_logs(f: impl FnOnce()) -> String {
        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::fmt().with_writer(capture.clone()).finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = capture.0.lock().clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// The frame is decoded into the real `Message` the reader forwards,
    /// not hand-built: an init payload that stopped carrying
    /// `mcp_servers` through `data` would leave the warn silent, which
    /// is the failure a test of the extractor alone cannot see.
    fn init_frame_with_a_failed_server() -> forge_primitives::Message {
        serde_json::from_value(serde_json::json!({
            "type": "system",
            "subtype": "init",
            "session_id": "sess-1",
            "model": "claude-opus-5[1m]",
            "mcp_servers": [
                {"name": "playwright", "status": "pending"},
                {"name": "jetbrains", "status": "failed"},
                {"name": "context7", "status": "connected"},
                {"name": "notion", "status": "needs-auth"},
            ],
        }))
        .expect("init frame decodes into Message")
    }

    #[test]
    fn a_failed_mcp_server_in_the_init_frame_is_logged_with_its_name() {
        let log = capture_logs(|| log_failed_mcp_servers(&init_frame_with_a_failed_server(), "s1"));
        assert!(log.contains("jetbrains"), "the record names the failed server: {log}");
        assert!(log.contains("s1"), "the record carries the resolved session id: {log}");
    }

    /// An init frame whose `tools` array carries the sdk server's tools
    /// under their `mcp__<name>__` prefixes - the healthy-boot shape.
    fn init_frame_with_forge_tools() -> serde_json::Value {
        serde_json::json!({
            "tools": [
                "Bash", "Read", "Edit",
                "mcp__forge__workers__tell",
                "mcp__forge__workers__ask",
            ],
        })
    }

    #[test]
    fn sdk_servers_missing_tools_is_true_when_no_tool_carries_the_prefix() {
        let no_forge = serde_json::json!({ "tools": ["Bash", "Read", "Edit"] });
        assert!(
            super::sdk_servers_missing_tools(&no_forge, &["forge".to_owned()]),
            "an init frame without mcp__forge__* tools means the sdk handshake was abandoned"
        );
    }

    #[test]
    fn sdk_servers_missing_tools_is_false_when_any_tool_carries_the_prefix() {
        assert!(
            !super::sdk_servers_missing_tools(
                &init_frame_with_forge_tools(),
                &["forge".to_owned()]
            ),
            "a healthy boot surfaces forge tools and needs no heal"
        );
    }

    #[test]
    fn an_empty_server_list_is_never_missing_tools() {
        let no_forge = serde_json::json!({ "tools": ["Bash", "Read"] });
        assert!(
            !super::sdk_servers_missing_tools(&no_forge, &[]),
            "a spawn with no sdk servers registered must never trigger the heal"
        );
    }

    #[test]
    fn sdk_servers_missing_tools_is_true_when_the_tools_array_is_absent() {
        let empty = serde_json::json!({});
        assert!(
            super::sdk_servers_missing_tools(&empty, &["forge".to_owned()]),
            "a missing tools array hides every sdk server's tools"
        );
    }

    /// Frames the heal gate must ignore, decoded from real wire shapes.
    /// Measured on the committed baselines: `hook_started` arrives BEFORE
    /// init, so a gate keyed on the wrong subtype would fire the heal
    /// mid-boot on healthy sessions.
    fn frame_gate_fixtures() -> (forge_primitives::Message, forge_primitives::Message) {
        let hook_started: forge_primitives::Message = serde_json::from_value(serde_json::json!({
            "type": "system",
            "subtype": "hook_started",
            "hook_id": "h-1",
            "hook_name": "SessionStart:startup",
            "hook_event": "SessionStart",
        }))
        .expect("hook_started frame decodes into Message");
        let assistant: forge_primitives::Message = serde_json::from_value(serde_json::json!({
            "type": "assistant",
            "session_id": "s-1",
            "message": {"id": "m-1", "role": "assistant", "model": "claude-opus-5",
                        "content": [{"type": "text", "text": "hi"}]},
        }))
        .expect("assistant frame decodes into Message");
        (hook_started, assistant)
    }

    #[test]
    fn the_heal_gate_fires_only_on_init_frames() {
        let names = ["forge".to_owned()];
        let init: forge_primitives::Message = serde_json::from_value(serde_json::json!({
            "type": "system",
            "subtype": "init",
            "session_id": "s-1",
            "tools": ["Bash"],
        }))
        .expect("init frame decodes into Message");
        let (hook_started, assistant) = frame_gate_fixtures();

        let mut evaluated = false;
        assert!(super::heal_due(&init, &mut evaluated, &names));

        let mut untouched = false;
        assert!(
            !super::heal_due(&hook_started, &mut untouched, &names),
            "a hook_started frame is not an init frame"
        );
        assert!(!untouched, "a non-init frame must not advance the latch");
        assert!(
            !super::heal_due(&assistant, &mut evaluated, &names),
            "an assistant frame is not an init frame"
        );
    }

    #[test]
    fn the_heal_fires_only_on_a_missing_boot_frame() {
        let names = ["forge".to_owned()];
        let missing = || {
            let msg: forge_primitives::Message = serde_json::from_value(serde_json::json!({
                "type": "system",
                "subtype": "init",
                "session_id": "s-1",
                "tools": ["Bash"],
            }))
            .expect("init frame decodes into Message");
            msg
        };
        let healed = || {
            let msg: forge_primitives::Message = serde_json::from_value(serde_json::json!({
                "type": "system",
                "subtype": "init",
                "session_id": "s-1",
                "tools": ["Bash", "mcp__forge__workers__tell"],
            }))
            .expect("init frame decodes into Message");
            msg
        };

        // Healthy boot: no fire, and a later init never re-evaluates.
        let mut evaluated = false;
        assert!(!super::heal_due(&healed(), &mut evaluated, &names));
        assert!(evaluated);
        assert!(
            !super::heal_due(&missing(), &mut evaluated, &names),
            "a mid-session init missing the tools must not fire the heal"
        );

        // Missing boot fires exactly once; later inits never re-fire.
        let mut evaluated = false;
        assert!(super::heal_due(&missing(), &mut evaluated, &names));
        assert!(evaluated);
        assert!(
            !super::heal_due(&missing(), &mut evaluated, &names),
            "the boot frame is evaluated once, whatever its verdict"
        );
        assert!(
            !super::heal_due(&healed(), &mut evaluated, &names),
            "a later init never fires the heal"
        );
    }

    #[test]
    fn servers_that_did_not_fail_are_not_logged() {
        let log = capture_logs(|| log_failed_mcp_servers(&init_frame_with_a_failed_server(), "s1"));
        for healthy in ["playwright", "context7", "notion"] {
            assert!(!log.contains(healthy), "{healthy} did not fail, so it is not logged: {log}");
        }
    }

    #[test]
    fn a_non_init_system_frame_logs_nothing() {
        let msg: forge_primitives::Message = serde_json::from_value(serde_json::json!({
            "type": "system",
            "subtype": "info",
            "session_id": "sess-1",
            "mcp_servers": [{"name": "jetbrains", "status": "failed"}],
        }))
        .expect("info frame decodes");
        let log = capture_logs(|| log_failed_mcp_servers(&msg, "s1"));
        assert!(log.is_empty(), "only the init handshake is scanned: {log}");
    }

    #[test]
    fn frame_session_id_prefers_own_then_live_then_spawn() {
        // A frame with its own id always wins.
        assert_eq!(frame_session_id(Some("real-uuid"), "live", "spawn"), "real-uuid");
        // Session-id-less frame on a bound session takes the live id -
        // this is the fresh-session fatal-Error case: spawn id is "".
        assert_eq!(frame_session_id(None, "live-uuid", ""), "live-uuid");
        // Before the live id binds, fall back to the spawn id.
        assert_eq!(frame_session_id(None, "", "spawn-uuid"), "spawn-uuid");
    }
    use crate::client::AgentEvent;
    use forge_primitives::ToolPermissionContext;
    use forge_primitives::permission_ui::PermissionAction;
    use forge_primitives::{PermissionOutcome, QuestionOutcome};
    use parking_lot::Mutex;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::oneshot;

    fn fresh_pending() -> PendingResponses {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn ctx(tool_name: &str, tool_use_id: &str, input: serde_json::Value) -> ToolPermissionContext {
        ToolPermissionContext::new(tool_name, input, tool_use_id, None)
    }

    fn park(
        pending: &PendingResponses,
        id: &str,
    ) -> oneshot::Receiver<forge_primitives::PermissionDecision> {
        let (tx, rx) = oneshot::channel();
        pending.lock().insert(id.to_owned(), tx);
        rx
    }

    #[test]
    fn reserved_env_keys_are_flagged_and_others_are_not() {
        // Iterate the const so it and the predicate stay in lockstep.
        for reserved in super::FORGE_RESERVED_ENV_KEYS {
            assert!(super::is_reserved_env_key(reserved), "{reserved} is forge-reserved");
        }
        // process.rs stamps CLAUDE_AGENT_SDK_VERSION last + unconditionally,
        // so an account env value can never override it - not reserved.
        assert!(!super::is_reserved_env_key("CLAUDE_AGENT_SDK_VERSION"));
        assert!(!super::is_reserved_env_key("ANTHROPIC_BASE_URL"));
        assert!(!super::is_reserved_env_key("ANTHROPIC_AUTH_TOKEN"));
        assert!(!super::is_reserved_env_key("ANTHROPIC_SMALL_FAST_MODEL"));
    }

    /// The wire between the two ends that were already pinned: the scan
    /// counts and the TUI seeds, but nothing checked that
    /// `load_history_messages` carries the number across unchanged. An
    /// off-by-one here ships green against both of those.
    #[test]
    fn load_history_messages_carries_the_transcript_count_across_verbatim() {
        use std::fmt::Write as _;

        let config_dir = tempfile::tempdir().expect("tempdir");
        let session_id = "550e8400-e29b-41d4-a716-446655440000";
        let project_dir = config_dir.path().join("projects").join("any-project-key");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        let mut jsonl = String::new();
        for i in 0..3 {
            let _ = writeln!(
                jsonl,
                "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"turn {i}\"}}}}"
            );
            jsonl.push_str(
                "{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"compactMetadata\":{\"trigger\":\"auto\",\"preTokens\":1002459}}\n",
            );
        }
        std::fs::write(project_dir.join(format!("{session_id}.jsonl")), jsonl).expect("write");

        // Empty cwd -> the scan searches every project dir for the file,
        // so the test does not depend on the cwd-to-project-key hash.
        let resumed = super::load_history_messages(config_dir.path(), session_id, "", session_id);

        assert_eq!(resumed.compaction_count, 3, "three boundaries reach the Connected event");
        assert_eq!(resumed.messages.len(), 3, "and the three turns still replay");
    }

    #[test]
    fn build_options_still_stamps_a_reserved_key_despite_the_warn() {
        // The collision warns but does not suppress the stamp: a
        // forge-reserved key in the merged env, whichever table
        // declared it, still lands on the child
        // (forge.toml is trusted, hand-authored).
        use crate::client::SessionLaunchSettings;
        use std::path::Path;
        use tokio::sync::mpsc;

        let (event_tx, _rx) = mpsc::unbounded_channel();
        let launch = SessionLaunchSettings::default();
        let mut env = HashMap::new();
        env.insert("CLAUDE_CONFIG_DIR".to_owned(), "/cfg/override".to_owned());
        assert!(super::is_reserved_env_key("CLAUDE_CONFIG_DIR"));
        let options = super::build_options_with_callback(
            "",
            None,
            &launch,
            event_tx,
            fresh_pending(),
            fresh_pending_questions(),
            Arc::new(Mutex::new(String::new())),
            Vec::new(),
            &super::AccountBinding { config_dir: Path::new("/cfg/x"), env: &env },
        );
        assert_eq!(options.env.get("CLAUDE_CONFIG_DIR").map(String::as_str), Some("/cfg/override"));
    }

    /// The workspace stamps an account's `permission_mode` into the
    /// launch settings' `permissions.defaultMode`; this is the arm that
    /// reads it, so the mode has to reach the OptionsBuilder from there.
    #[test]
    fn launch_settings_default_mode_reaches_the_options_builder() {
        use crate::client::SessionLaunchSettings;
        use forge_primitives::permission::PermissionMode;
        use std::path::Path;
        use tokio::sync::mpsc;

        let (event_tx, _rx) = mpsc::unbounded_channel();
        let launch = SessionLaunchSettings {
            settings: Some(json!({
                "permissions": { "defaultMode": "bypassPermissions" },
                "model": "haiku",
            })),
            ..SessionLaunchSettings::default()
        };
        let options = super::build_options_with_callback(
            "",
            None,
            &launch,
            event_tx,
            fresh_pending(),
            fresh_pending_questions(),
            Arc::new(Mutex::new(String::new())),
            Vec::new(),
            &super::AccountBinding { config_dir: Path::new("/cfg/x"), env: &HashMap::new() },
        );
        assert_eq!(
            options.permission_mode,
            PermissionMode::BypassPermissions,
            "settings.permissions.defaultMode drives the CLI's permission mode",
        );
        assert_eq!(
            options.model.as_deref(),
            Some("haiku"),
            "sibling settings survive the mode arm"
        );
    }

    /// Launch settings without a `defaultMode` leave the builder at its
    /// default - accounts (and callers) that never set the key spawn
    /// exactly as before.
    #[test]
    fn launch_settings_without_a_default_mode_leave_the_builder_default() {
        use crate::client::SessionLaunchSettings;
        use forge_primitives::permission::PermissionMode;
        use std::path::Path;
        use tokio::sync::mpsc;

        let (event_tx, _rx) = mpsc::unbounded_channel();
        let launch = SessionLaunchSettings::default();
        let options = super::build_options_with_callback(
            "",
            None,
            &launch,
            event_tx,
            fresh_pending(),
            fresh_pending_questions(),
            Arc::new(Mutex::new(String::new())),
            Vec::new(),
            &super::AccountBinding { config_dir: Path::new("/cfg/x"), env: &HashMap::new() },
        );
        assert_eq!(options.permission_mode, PermissionMode::Ask);
    }

    #[test]
    fn bypass_init_record_seeds_the_connected_mode_list() {
        let init_record = serde_json::json!({ "permissionMode": "bypassPermissions" });
        let state = initial_mode_state(init_record.as_object(), None, false);
        assert!(
            state.available_modes.iter().any(|m| m.id == "bypassPermissions"),
            "bypass launch offers bypass at connect: {:?}",
            state.available_modes,
        );
    }

    #[test]
    fn build_options_stamps_account_env_and_config_dir() {
        use crate::client::SessionLaunchSettings;
        use std::path::Path;
        use tokio::sync::mpsc;

        let (event_tx, _rx) = mpsc::unbounded_channel();
        let launch = SessionLaunchSettings::default();
        let mut env = HashMap::new();
        env.insert("ANTHROPIC_BASE_URL".to_owned(), "http://localhost:18765".to_owned());
        env.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), "unused".to_owned());
        let options = super::build_options_with_callback(
            "",
            None,
            &launch,
            event_tx,
            fresh_pending(),
            fresh_pending_questions(),
            Arc::new(Mutex::new(String::new())),
            Vec::new(),
            &super::AccountBinding { config_dir: Path::new("/cfg/codex"), env: &env },
        );
        assert_eq!(
            options.env.get("ANTHROPIC_BASE_URL").map(String::as_str),
            Some("http://localhost:18765"),
        );
        assert_eq!(options.env.get("ANTHROPIC_AUTH_TOKEN").map(String::as_str), Some("unused"));
        assert_eq!(
            options.env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
            Some("/cfg/codex"),
            "bound account config_dir still stamped alongside the account env",
        );
    }

    /// A `UserPromptSubmit` callback hook puts forge's control-stream
    /// round-trip on the prompt-submission path, and the CLI answers a
    /// callback timeout there with `suppressOriginalPrompt` - the user's
    /// prompt is discarded. Observation belongs on `PreToolUse`, which
    /// carries the same `permission_mode` / `effort` plus subagent
    /// attribution.
    #[test]
    fn observation_hooks_stay_off_the_prompt_submit_path() {
        use crate::client::SessionLaunchSettings;
        use std::path::Path;
        use tokio::sync::mpsc;

        let (event_tx, _rx) = mpsc::unbounded_channel();
        let launch = SessionLaunchSettings::default();
        let env = HashMap::new();
        let options = super::build_options_with_callback(
            "",
            None,
            &launch,
            event_tx,
            fresh_pending(),
            fresh_pending_questions(),
            Arc::new(Mutex::new(String::new())),
            Vec::new(),
            &super::AccountBinding { config_dir: Path::new("/cfg/x"), env: &env },
        );

        let desc = format!("{options:?}");
        assert!(
            desc.contains("user_prompt_submit_count: 0"),
            "no UserPromptSubmit hook may be registered, or a slow reply blocks the prompt: {desc}",
        );
        assert!(
            desc.contains("pre_tool_use_count: 1"),
            "the PreToolUse observation hook is still registered: {desc}",
        );
    }

    #[test]
    fn system_prompt_orders_trust_cron_catalog_charter() {
        let out = build_forge_system_prompt(true, Some("CATALOG"), Some("CHARTER"));
        let trust_at = out.find("in-process forge MCP").expect("trust present");
        let cron_at = out.find("cron__create").expect("cron present");
        let cat_at = out.find("CATALOG").expect("catalog present");
        let chr_at = out.find("CHARTER").expect("charter present");
        assert!(
            trust_at < cron_at && cron_at < cat_at && cat_at < chr_at,
            "order: trust, cron, catalog, charter"
        );

        let bare = build_forge_system_prompt(true, None, None);
        assert!(bare.contains("in-process forge MCP"));
        assert!(bare.contains("cron__create"), "cron scheduling is always present");
        let conduct_at = out.find("`description`").expect("conduct present");
        assert!(
            cron_at < conduct_at && conduct_at < cat_at,
            "order: trust, cron, conduct, catalog, charter"
        );
        assert!(
            bare.contains("`description`"),
            "the Bash description ask rides the base append, not a charter"
        );
        assert!(!bare.contains("CATALOG"));
    }

    #[test]
    fn system_prompt_names_peers_tools_only_when_registered() {
        let lead = build_forge_system_prompt(true, None, None);
        for tool in ["peers__whoami", "peers__list_agents", "peers__tell_agent", "peers__ask_agent"]
        {
            assert!(lead.contains(tool), "lead append names {tool}: {lead}");
        }

        let worker = build_forge_system_prompt(false, None, None);
        assert!(
            !worker.contains("peer-coordination tools"),
            "worker append must not claim peers tools it lacks: {worker}"
        );
        assert!(worker.contains("bracket envelopes"), "envelope trust stays: {worker}");
        assert!(worker.contains("cron__create"), "cron block stays: {worker}");
        assert!(worker.contains("`description`"), "conduct block stays: {worker}");
    }

    /// The gate reads the registered server's own tool surface, not a
    /// parallel flag: a worker-shaped `forge` server (no peers tools)
    /// must yield an append that never claims them.
    #[test]
    fn forge_server_tool_surface_drives_the_peers_paragraph() {
        use crate::client::SessionLaunchSettings;
        use forge_sdk::mcp::{McpServerBuilder, Tool, ToolInput, ToolOutput};
        use std::path::Path;
        use tokio::sync::mpsc;

        struct NamedTool(&'static str);

        #[async_trait::async_trait]
        impl Tool for NamedTool {
            fn name(&self) -> &'static str {
                self.0
            }
            fn description(&self) -> &'static str {
                "probe tool"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false})
            }
            async fn call(&self, _input: ToolInput) -> ToolOutput {
                ToolOutput::text("ok")
            }
        }

        let append_of = |servers: Vec<(String, forge_sdk::mcp::McpServer)>| {
            let (event_tx, _rx) = mpsc::unbounded_channel();
            let launch = SessionLaunchSettings::default();
            let options = super::build_options_with_callback(
                "",
                None,
                &launch,
                event_tx,
                fresh_pending(),
                fresh_pending_questions(),
                Arc::new(Mutex::new(String::new())),
                servers,
                &super::AccountBinding { config_dir: Path::new("/cfg/x"), env: &HashMap::new() },
            );
            match options.system_prompt {
                Some(forge_sdk::SystemPromptKind::Preset { append: Some(text), .. }) => text,
                other => panic!("expected a Preset append, got {other:?}"),
            }
        };

        let lead_server = McpServerBuilder::new("forge", "0.0.0")
            .tool(NamedTool("peers__whoami"))
            .tool(NamedTool("workers__tell"))
            .build();
        let worker_server =
            McpServerBuilder::new("forge", "0.0.0").tool(NamedTool("workers__tell")).build();

        let lead = append_of(vec![("forge".to_owned(), lead_server)]);
        assert!(lead.contains("peer-coordination tools"), "lead append: {lead}");
        let worker = append_of(vec![("forge".to_owned(), worker_server)]);
        assert!(
            !worker.contains("peer-coordination tools"),
            "worker append must not claim peers tools: {worker}"
        );
    }

    #[test]
    fn exclude_dynamic_sections_defaults_off() {
        // Default-off maps to None so the forge spawn's Preset omits the
        // `excludeDynamicSections` initialize key (wire byte-identical to
        // today). Enabling is a deliberate const flip that must update
        // this assertion too.
        let field = super::EXCLUDE_DYNAMIC_SECTIONS.then_some(true);
        assert_eq!(field, None);
    }

    #[test]
    fn permission_response_allow_drains_oneshot_with_allow() {
        let pending = fresh_pending();
        let rx = park(&pending, "tu_1");
        deliver_permission_response(
            &pending,
            "tu_1",
            PermissionOutcome::Selected {
                option_id: "allow_once".to_owned(),
                action: PermissionAction::Allow,
                notes_text: None,
                edited_input: None,
            },
        );
        let decision = rx.blocking_recv().expect("oneshot resolved");
        assert!(decision.is_allow());
    }

    #[test]
    fn permission_response_deny_keyword_drains_with_deny() {
        let pending = fresh_pending();
        let rx = park(&pending, "tu_2");
        deliver_permission_response(
            &pending,
            "tu_2",
            PermissionOutcome::Selected {
                option_id: "deny".to_owned(),
                action: PermissionAction::Deny,
                notes_text: None,
                edited_input: None,
            },
        );
        let decision = rx.blocking_recv().expect("oneshot resolved");
        assert!(!decision.is_allow());
    }

    #[test]
    fn permission_response_reject_once_drains_with_deny() {
        let pending = fresh_pending();
        let rx = park(&pending, "tu_r1");
        deliver_permission_response(
            &pending,
            "tu_r1",
            PermissionOutcome::Selected {
                option_id: "reject_once".to_owned(),
                action: PermissionAction::Deny,
                notes_text: None,
                edited_input: None,
            },
        );
        let decision = rx.blocking_recv().expect("oneshot resolved");
        assert!(!decision.is_allow());
    }

    #[test]
    fn permission_response_reject_always_drains_with_deny() {
        let pending = fresh_pending();
        let rx = park(&pending, "tu_r2");
        deliver_permission_response(
            &pending,
            "tu_r2",
            PermissionOutcome::Selected {
                option_id: "reject_always".to_owned(),
                action: PermissionAction::Deny,
                notes_text: None,
                edited_input: None,
            },
        );
        let decision = rx.blocking_recv().expect("oneshot resolved");
        assert!(!decision.is_allow());
    }

    #[test]
    fn permission_response_cancel_drains_with_deny() {
        let pending = fresh_pending();
        let rx = park(&pending, "tu_3");
        deliver_permission_response(&pending, "tu_3", PermissionOutcome::Cancelled);
        let decision = rx.blocking_recv().expect("oneshot resolved");
        assert!(!decision.is_allow());
    }

    #[test]
    fn permission_response_unknown_id_is_silent_no_op() {
        let pending = fresh_pending();
        deliver_permission_response(
            &pending,
            "missing",
            PermissionOutcome::Selected {
                option_id: "allow_once".to_owned(),
                action: PermissionAction::Allow,
                notes_text: None,
                edited_input: None,
            },
        );
        assert!(pending.lock().is_empty());
    }

    fn fresh_pending_questions() -> PendingQuestions {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn park_question(
        pending: &PendingQuestions,
        id: &str,
    ) -> oneshot::Receiver<forge_primitives::QuestionOutcome> {
        let (tx, rx) = oneshot::channel();
        pending.lock().insert(id.to_owned(), tx);
        rx
    }

    #[test]
    fn question_response_forwards_typed_outcome() {
        let pending = fresh_pending_questions();
        let rx = park_question(&pending, "tu_q1");
        deliver_question_response(
            &pending,
            "tu_q1",
            QuestionOutcome::Answered {
                selected_option_ids: vec!["question_0".to_owned(), "question_1".to_owned()],
                annotation: None,
            },
        );
        let outcome = rx.blocking_recv().expect("oneshot resolved");
        match outcome {
            QuestionOutcome::Answered { selected_option_ids, .. } => {
                assert_eq!(selected_option_ids, vec!["question_0", "question_1"]);
            }
            QuestionOutcome::Cancelled => panic!("expected answered outcome"),
        }
    }

    #[test]
    fn question_response_cancelled_forwards_typed_outcome() {
        let pending = fresh_pending_questions();
        let rx = park_question(&pending, "tu_q2");
        deliver_question_response(&pending, "tu_q2", QuestionOutcome::Cancelled);
        let outcome = rx.blocking_recv().expect("oneshot resolved");
        assert!(matches!(outcome, QuestionOutcome::Cancelled));
    }

    #[test]
    fn question_response_unknown_id_is_silent_no_op() {
        let pending = fresh_pending_questions();
        deliver_question_response(&pending, "missing", QuestionOutcome::Cancelled);
        assert!(pending.lock().is_empty());
    }

    /// The CLI looks answers up by the question text EXACTLY as it
    /// arrived on the wire, so trimming that string for the answers/
    /// annotations keys silently loses the answer.
    #[tokio::test]
    async fn ask_user_question_keys_answers_by_original_wire_text() {
        let dirty_pick = "\r\r#887: pick one  ";
        let dirty_note = " notes only?\r";
        let input = json!({"questions": [
            {
                "question": dirty_pick,
                "header": "H",
                "options": [
                    {"label": "One", "description": "", "preview": "p1"},
                    {"label": "Two", "description": ""},
                ],
            },
            {
                "question": dirty_note,
                "header": "H",
                "options": [
                    {"label": "Yes", "description": ""},
                    {"label": "No", "description": ""},
                ],
            },
        ]});
        let (event_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        let pending = fresh_pending_questions();
        let task_pending = pending.clone();
        let task = tokio::spawn(async move {
            run_ask_user_question(
                ctx("AskUserQuestion", "tu_keys", input),
                "sess".to_owned(),
                &event_tx,
                &task_pending,
            )
            .await
        });

        match events.recv().await.expect("first question request") {
            AgentEvent::QuestionRequest { request, .. } => {
                assert_eq!(request.question_index, 0);
                deliver_question_response(
                    &pending,
                    "tu_keys",
                    QuestionOutcome::Answered {
                        selected_option_ids: vec!["question_0".to_owned()],
                        annotation: None,
                    },
                );
            }
            other => panic!("expected QuestionRequest, got {other:?}"),
        }
        match events.recv().await.expect("second question request") {
            AgentEvent::QuestionRequest { request, .. } => {
                assert_eq!(request.question_index, 1);
                deliver_question_response(
                    &pending,
                    "tu_keys",
                    QuestionOutcome::Answered {
                        selected_option_ids: Vec::new(),
                        annotation: Some(forge_primitives::QuestionAnnotation {
                            preview: None,
                            notes: Some("keep both".to_owned()),
                        }),
                    },
                );
            }
            other => panic!("expected QuestionRequest, got {other:?}"),
        }

        let decision = task.await.expect("ask task joins");
        assert!(decision.is_allow(), "expected allow, got {decision:?}");
        let updated = decision.updated_input().expect("updated input present");
        let questions =
            updated.get("questions").and_then(serde_json::Value::as_array).expect("questions");
        assert_eq!(
            questions[0].get("question").and_then(serde_json::Value::as_str),
            Some(dirty_pick),
            "questions array must keep the original wire text"
        );
        assert_eq!(
            questions[1].get("question").and_then(serde_json::Value::as_str),
            Some(dirty_note),
            "questions array must keep the original wire text"
        );
        let answers =
            updated.get("answers").and_then(serde_json::Value::as_object).expect("answers");
        assert_eq!(answers.len(), 2, "both answers present");
        assert_eq!(answers.get(dirty_pick).and_then(serde_json::Value::as_str), Some("One"));
        assert_eq!(answers.get(dirty_note).and_then(serde_json::Value::as_str), Some("Other"));
        let annotations =
            updated.get("annotations").and_then(serde_json::Value::as_object).expect("annotations");
        assert_eq!(
            annotations
                .get(dirty_pick)
                .and_then(|a| a.get("preview"))
                .and_then(serde_json::Value::as_str),
            Some("p1"),
            "selected-option annotation keyed by the original wire text"
        );
        assert_eq!(
            annotations
                .get(dirty_note)
                .and_then(|a| a.get("notes"))
                .and_then(serde_json::Value::as_str),
            Some("keep both"),
            "notes annotation keyed by the original wire text"
        );
    }

    #[test]
    fn synth_permission_request_carries_tool_input_and_display_fields() {
        let c = ctx("Bash", "tu_p1", json!({ "command": "ls" })).with_display(
            None,
            None,
            Some("Run shell command".to_owned()),
            Some("Bash".to_owned()),
            Some("Lists directory entries".to_owned()),
        );
        let event = synth_permission_request("sess_1", &c);
        let AgentEvent::PermissionRequest { session_id, request } = event else {
            panic!("expected PermissionRequest");
        };
        assert_eq!(session_id, "sess_1");
        assert_eq!(request.tool_call.tool_call_id, "tu_p1");
        assert_eq!(request.tool_call.title, "Bash");
        assert_eq!(request.tool_call.raw_input, Some(json!({ "command": "ls" })));
        // Empty suggestions + editable tool (Bash) -> [allow_once,
        // allow_with_edits, deny, tell_claude]. Shape assertion: first
        // is allow_once, last is tell_claude, deny present.
        let ids: Vec<&str> = request.options.iter().map(|o| o.option_id.as_str()).collect();
        assert_eq!(ids.first().copied(), Some("allow_once"));
        assert_eq!(ids.last().copied(), Some("tell_claude"));
        assert!(ids.contains(&"deny"));
        let display = request.display.expect("display populated");
        assert_eq!(display.title.as_deref(), Some("Run shell command"));
        assert_eq!(display.display_name.as_deref(), Some("Bash"));
        assert_eq!(display.description.as_deref(), Some("Lists directory entries"));
    }

    #[test]
    fn synth_permission_request_surfaces_decision_reason() {
        let c = ctx("Read", "tu_dr1", json!({ "file_path": "/tmp/x" })).with_display(
            None,
            Some("Path is outside allowed working directories".to_owned()),
            None,
            None,
            None,
        );
        let event = synth_permission_request("session-1", &c);
        let AgentEvent::PermissionRequest { request, .. } = event else {
            panic!("expected PermissionRequest");
        };
        let display = request.display.expect("display populated");
        assert_eq!(
            display.decision_reason.as_deref(),
            Some("Path is outside allowed working directories"),
        );
    }

    #[test]
    fn pending_lock_remove_drains_entry() {
        let pending = fresh_pending();
        let _rx = park(&pending, "tu_x");
        assert!(pending.lock().remove("tu_x").is_some());
        assert!(pending.lock().remove("tu_x").is_none());
    }
}

#[cfg(test)]
mod tests_permission_options {
    use super::{build_permission_options, dispatch_permission_action};
    use forge_primitives::options::PermissionMode;
    use forge_primitives::permission_ui::{PermissionAction, PermissionOptionKind};
    use forge_primitives::permissions::{
        PermissionBehavior, PermissionRuleValue, PermissionUpdate, PermissionUpdateDestination,
        ToolPermissionContext,
    };
    use serde_json::json;

    fn mk_ctx(tool_name: &str, suggestions: Vec<PermissionUpdate>) -> ToolPermissionContext {
        ToolPermissionContext::new(tool_name, json!({}), "tu-1", None).with_suggestions(suggestions)
    }

    #[test]
    fn empty_suggestions_yields_baseline_options() {
        // Non-editable tool (Read), no suggestions: Allow once, Deny, Tell Claude.
        let opts = build_permission_options(&mk_ctx("Read", vec![]));
        let ids: Vec<&str> = opts.iter().map(|o| o.option_id.as_str()).collect();
        assert_eq!(ids, vec!["allow_once", "deny", "tell_claude"]);
    }

    #[test]
    fn editable_tool_appends_allow_with_edits() {
        let opts = build_permission_options(&mk_ctx("Bash", vec![]));
        let ids: Vec<&str> = opts.iter().map(|o| o.option_id.as_str()).collect();
        assert_eq!(ids, vec!["allow_once", "allow_with_edits", "deny", "tell_claude"]);
        let edits = opts
            .iter()
            .find(|o| o.option_id == "allow_with_edits")
            .expect("allow_with_edits present for editable tool");
        assert_eq!(edits.kind, PermissionOptionKind::Edit);
        assert_eq!(edits.action, PermissionAction::AllowWithInput);
    }

    #[test]
    fn add_rules_suggestion_inserts_allow_always_option() {
        let suggestion = PermissionUpdate::AddRules {
            rules: vec![PermissionRuleValue {
                tool_name: "Read".into(),
                rule_content: Some("//tmp/**".into()),
            }],
            behavior: PermissionBehavior::Allow,
            destination: Some(PermissionUpdateDestination::Session),
        };
        let opts = build_permission_options(&mk_ctx("Read", vec![suggestion]));
        // Order: allow_once, allow_always_0 (rules), deny, tell_claude.
        let ids: Vec<&str> = opts.iter().map(|o| o.option_id.as_str()).collect();
        assert_eq!(ids, vec!["allow_once", "allow_always_0", "deny", "tell_claude"]);
        let allow_always =
            opts.iter().find(|o| o.option_id == "allow_always_0").expect("allow_always_0 present");
        assert_eq!(allow_always.kind, PermissionOptionKind::Allow);
        assert!(matches!(allow_always.action, PermissionAction::AllowWithUpdates { .. }));
    }

    #[test]
    fn macos_tmp_mirror_suggestions_collapse_for_display_but_keep_both_rules() {
        let suggestion_a = PermissionUpdate::AddRules {
            rules: vec![PermissionRuleValue {
                tool_name: "Read".into(),
                rule_content: Some("//tmp/**".into()),
            }],
            behavior: PermissionBehavior::Allow,
            destination: Some(PermissionUpdateDestination::Session),
        };
        let suggestion_b = PermissionUpdate::AddRules {
            rules: vec![PermissionRuleValue {
                tool_name: "Read".into(),
                rule_content: Some("//private/tmp/**".into()),
            }],
            behavior: PermissionBehavior::Allow,
            destination: Some(PermissionUpdateDestination::Session),
        };
        let opts = build_permission_options(&mk_ctx("Read", vec![suggestion_a, suggestion_b]));
        // ONE "Allow always" option whose action carries BOTH rules - not
        // two separate "Allow always" entries.
        let allow_always_count =
            opts.iter().filter(|o| o.option_id.starts_with("allow_always_")).count();
        assert_eq!(
            allow_always_count, 1,
            "macOS /tmp + /private/tmp should collapse into one option"
        );
        let merged = opts
            .iter()
            .find(|o| o.option_id.starts_with("allow_always_"))
            .expect("merged allow_always option present");
        let PermissionAction::AllowWithUpdates { updates } = &merged.action else {
            panic!("expected AllowWithUpdates");
        };
        // Both rules should be present in the merged update - count rule
        // entries across all updates.
        let total_rules: usize = updates
            .iter()
            .map(|u| match u {
                PermissionUpdate::AddRules { rules, .. } => rules.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(total_rules, 2, "merged action should carry both /tmp and /private/tmp rules");
    }

    #[test]
    fn add_directories_suggestion_yields_allow_always_option() {
        let suggestion = PermissionUpdate::AddDirectories {
            directories: vec!["/tmp".into(), "/private/tmp".into()],
            destination: Some(PermissionUpdateDestination::Session),
        };
        let opts = build_permission_options(&mk_ctx("Write", vec![suggestion]));
        let ids: Vec<&str> = opts.iter().map(|o| o.option_id.as_str()).collect();
        // Write is editable so allow_with_edits also appears.
        assert!(ids.contains(&"allow_always_0"));
        assert!(ids.contains(&"allow_with_edits"));
    }

    #[test]
    fn set_mode_suggestion_promotes_accept_edits_to_auto() {
        // Forge promotes the wire's `acceptEdits` SetMode suggestion
        // to `auto` so the user sees a single coherent Auto/Ask toggle
        // instead of a partial-auto stepping stone.
        let suggestion = PermissionUpdate::SetMode {
            mode: PermissionMode::AcceptEdits,
            destination: Some(PermissionUpdateDestination::Session),
        };
        let opts = build_permission_options(&mk_ctx("Write", vec![suggestion]));
        let switch_mode =
            opts.iter().find(|o| o.option_id == "allow_always_0").expect("allow_always_0 present");
        assert!(
            switch_mode.name.to_lowercase().contains("auto"),
            "expected 'Auto' in promoted option name; got: {}",
            switch_mode.name
        );
        // Action carries the swapped SetMode with Auto target.
        let PermissionAction::AllowWithUpdates { updates } = &switch_mode.action else {
            panic!("expected AllowWithUpdates action; got {:?}", switch_mode.action);
        };
        let PermissionUpdate::SetMode { mode, .. } = &updates[0] else {
            panic!("expected SetMode update");
        };
        assert_eq!(*mode, PermissionMode::Auto);
    }

    #[test]
    fn set_mode_suggestion_keeps_non_accept_edits_modes() {
        // Plan / Ask / Auto / BypassPermissions targets pass through
        // unchanged - only acceptEdits is promoted.
        let suggestion =
            PermissionUpdate::SetMode { mode: PermissionMode::Plan, destination: None };
        let opts = build_permission_options(&mk_ctx("Write", vec![suggestion]));
        let switch_mode =
            opts.iter().find(|o| o.option_id == "allow_always_0").expect("allow_always_0 present");
        let PermissionAction::AllowWithUpdates { updates } = &switch_mode.action else {
            panic!("expected AllowWithUpdates");
        };
        let PermissionUpdate::SetMode { mode, .. } = &updates[0] else {
            panic!("expected SetMode");
        };
        assert_eq!(*mode, PermissionMode::Plan);
    }

    #[test]
    fn dispatch_allow_action_yields_allow_decision() {
        let decision = dispatch_permission_action(PermissionAction::Allow, "", None);
        assert!(decision.is_allow());
    }

    #[test]
    fn dispatch_deny_with_notes_passes_notes_as_reason() {
        let decision =
            dispatch_permission_action(PermissionAction::Deny, "use --dry-run first", None);
        let reason = decision.reason().expect("deny carries reason");
        assert_eq!(reason, "use --dry-run first");
    }

    #[test]
    fn dispatch_deny_with_empty_notes_uses_default_reason() {
        let decision = dispatch_permission_action(PermissionAction::Deny, "", None);
        let reason = decision.reason().expect("deny carries reason");
        assert_eq!(reason, "Denied by user");
    }

    #[test]
    fn dispatch_allow_with_updates_attaches_them() {
        let updates =
            vec![PermissionUpdate::SetMode { mode: PermissionMode::Auto, destination: None }];
        let decision = dispatch_permission_action(
            PermissionAction::AllowWithUpdates { updates: updates.clone() },
            "",
            None,
        );
        assert_eq!(decision.updated_permissions(), updates.as_slice());
    }

    #[test]
    fn dispatch_allow_with_input_uses_edited_value() {
        let edited = json!({"command": "echo modified"});
        let decision =
            dispatch_permission_action(PermissionAction::AllowWithInput, "", Some(edited.clone()));
        let updated_input = decision.updated_input().expect("allow_with_input carries value");
        assert_eq!(updated_input, &edited);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests_reader_terminal {
    use super::reader_loop;
    use super::set_test_sdk_binary;
    use crate::client::AgentEvent;
    use crate::forge_sdk_bridge::ForgeSdkBridge;
    use forge_sdk::{Client, OptionsBuilder};
    use std::collections::HashMap;

    /// forge-sdk's shared dev fixture. reader_loop only needs a
    /// completed handshake to hold a `Client`; the stream it drains is
    /// a private channel the test drives directly.
    fn sdk_mock_binary() -> String {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../forge-sdk/tests/fixtures/mock_claude.sh")
            .to_owned()
    }

    async fn mock_client() -> (Client, forge_sdk::ClientEvents) {
        let opts = OptionsBuilder::new().binary(sdk_mock_binary()).build();
        Client::spawn(opts).await.expect("mock client spawns")
    }

    /// A stream ERROR must surface as `AgentEvent::ConnectionFailed`
    /// before `reader_loop` returns - the only terminal signal the
    /// consumer gets; log-only death leaves the spinner and the turn
    /// pending forever.
    #[tokio::test]
    async fn reader_loop_emits_connection_failed_on_stream_error() {
        let (client, _events) = mock_client().await;
        let (stream_tx, stream_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, mut observed) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        stream_tx
            .send(Err(forge_sdk::Error::Connection { reason: "pipe broke".into() }))
            .expect("seed a stream error");

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reader_loop(stream_rx, event_tx, "spawn-id".to_owned(), client, Vec::new()),
        )
        .await
        .expect("reader_loop returns after a stream error");

        match observed.try_recv() {
            Ok(AgentEvent::ConnectionFailed { message }) => {
                assert!(
                    message.contains("pipe broke"),
                    "the failure message carries the stream error: {message}"
                );
            }
            other => panic!("expected ConnectionFailed on stream error, got {other:?}"),
        }
        assert!(observed.try_recv().is_err(), "no further events after the terminal one");
    }

    /// A clean stream CLOSE (EOF drain, e.g. the child exited) is the
    /// same terminal: emit, then return.
    #[tokio::test]
    async fn reader_loop_emits_connection_failed_on_stream_close() {
        let (client, _events) = mock_client().await;
        let (events_tx, events) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, mut observed) = tokio::sync::mpsc::unbounded_channel();
        drop(events_tx);

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reader_loop(events, event_tx, "spawn-id".to_owned(), client, Vec::new()),
        )
        .await
        .expect("reader_loop returns after the stream closes");

        match observed.try_recv() {
            Ok(AgentEvent::ConnectionFailed { message }) => {
                assert!(!message.trim().is_empty(), "the close arm still carries a message");
            }
            other => panic!("expected ConnectionFailed on stream close, got {other:?}"),
        }
    }

    /// A self-initiated disconnect is not a death. A session swap
    /// disconnects the previous client while a NEW spawn emits its own
    /// `Connected` - the old reader's stream end is the expected
    /// consequence, and it must reach the workspace as silence, not as
    /// a task-terminal `ConnectionFailed` (which would release the
    /// pool mid-swap and disconnect the freshly spawned client).
    #[tokio::test]
    async fn session_swap_does_not_emit_connection_failed() {
        set_test_sdk_binary(Some(sdk_mock_binary()));
        let dir = tempfile::tempdir().expect("tempdir");
        let bridge = ForgeSdkBridge::new(dir.path().to_owned(), None, Vec::new(), HashMap::new());
        let mut events = bridge.take_events().expect("fresh bridge yields its events receiver");

        bridge
            .new_session("/tmp".to_owned(), crate::client::SessionLaunchSettings::default())
            .expect("dispatch");

        // First spawn: Connected#1. The mock completes its handshake
        // in real time.
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .expect("Connected#1 arrives promptly");
        assert!(
            matches!(first, Some(AgentEvent::Connected { .. })),
            "expected Connected#1, got {first:?}"
        );

        // Second spawn on the same bridge: the swap. clear_client takes
        // client#1, disconnect() runs to completion (SDK reader joined),
        // client#2 spawns, Connected#2 emits. Interleaved events
        // (status snapshots etc.) are fine; a ConnectionFailed between
        // the two Connecteds is not.
        bridge
            .new_session("/tmp".to_owned(), crate::client::SessionLaunchSettings::default())
            .expect("dispatch");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(event) = events.recv().await {
                match event {
                    AgentEvent::Connected { .. } => break,
                    AgentEvent::ConnectionFailed { message } => {
                        panic!("a swap must not emit ConnectionFailed: {message}");
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("Connected#2 arrives promptly");

        // Client#1's reader_loop exits as a consequence of the
        // disconnect; give it a moment to be scheduled, then assert it
        // stayed SILENT - no ConnectionFailed, nothing at all.
        tokio::time::timeout(std::time::Duration::from_millis(300), async {
            loop {
                match tokio::time::timeout(std::time::Duration::from_millis(100), events.recv())
                    .await
                {
                    Ok(Some(AgentEvent::ConnectionFailed { message })) => {
                        panic!("a swap must not emit ConnectionFailed: {message}");
                    }
                    Ok(Some(_)) => {}
                    // Ok(None) or timeout: either means silence held.
                    _ => break,
                }
            }
        })
        .await
        .expect("drain window completes");

        set_test_sdk_binary(None);
    }
}
