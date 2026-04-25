//! Public [`Client`] — the entry point consumers hold.
//!
//! The `Client` struct is defined here along with its lifecycle methods
//! ([`spawn`](Client::spawn), [`next_event`](Client::next_event),
//! [`send_user_message`](Client::send_user_message),
//! [`disconnect`](Client::disconnect)). Inbound `control_request`
//! dispatching lives in [`control_dispatch`]; outbound `control_request`
//! issuance lives in [`control_send`].

mod control_dispatch;
mod control_send;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use tracing::debug;

use crate::Error;
use crate::hooks::ErasedHookCallback;
use crate::mcp::orchestration::McpHosts;
use crate::messages::Message;
use crate::options::Options;
use crate::permissions::CanUseToolCallback;
use crate::transport::Transport;
use crate::transport::codec::{DecodedLine, decode_dispatch, decode_line};
use crate::transport::process::Subprocess;

/// An active `claude` binary subprocess.
///
/// Construct via [`spawn`](Self::spawn). The init handshake
/// ([`initialize` `control_request`](https://github.com/anthropics/claude-agent-sdk-python/blob/main/src/claude_agent_sdk/_internal/query.py#L196-L214))
/// runs inside `spawn`; the `system/init` frame is consumed and its
/// session id cached on [`session_id()`](Self::session_id). Callers
/// of [`next_event`](Self::next_event) see the clean conversational
/// stream starting with hook/assistant/user frames.
///
/// The transport is held as a `Box<dyn Transport>` so callers can inject
/// an alternative I/O backend via
/// [`spawn_with_transport`](Self::spawn_with_transport).
pub struct Client {
    pub(crate) sub: Box<dyn Transport>,
    pub(crate) session_id: String,
    pub(crate) line_number: u64,
    pub(crate) can_use_tool: Option<Arc<dyn CanUseToolCallback>>,
    pub(crate) mcp_hosts: McpHosts,
    pub(crate) hook_callbacks: HashMap<String, Arc<dyn ErasedHookCallback>>,
    /// Messages read off the transport BEFORE the `system/init` frame
    /// arrived. The CLI may emit non-init system frames first (e.g.
    /// `hook_started` for a `SessionStart` settings-file hook) — we
    /// buffer these here and drain them through `next_event` in order
    /// so callers observe them as part of the normal message stream.
    pub(crate) pre_init_messages: VecDeque<Message>,
    /// Response payload from the `initialize` `control_request`, cached so
    /// [`get_server_info`](Self::get_server_info) can return it without
    /// re-issuing the handshake. Python stores the same payload at
    /// `_internal/query.py:214` (`self._initialization_result`).
    pub(crate) initialization_result: Option<serde_json::Value>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("sub", &"<transport>")
            .field("session_id", &self.session_id)
            .field("line_number", &self.line_number)
            .field(
                "can_use_tool",
                &self.can_use_tool.as_ref().map(|_| "<callback>"),
            )
            .field("mcp_hosts", &self.mcp_hosts)
            .field(
                "hook_callbacks",
                &format!("<{} hooks>", self.hook_callbacks.len()),
            )
            .field(
                "pre_init_messages",
                &format!("<{} buffered>", self.pre_init_messages.len()),
            )
            .field(
                "initialization_result",
                &self.initialization_result.as_ref().map(|_| "<cached>"),
            )
            .finish()
    }
}

impl Client {
    /// Spawn `claude` with the given options and drain the init line.
    ///
    /// # Errors
    ///
    /// Any [`Error`] variant; see field docs.
    pub async fn spawn(options: Options) -> Result<Self, Error> {
        let sub = Subprocess::spawn(&options).await?;
        Self::spawn_inner(options, Box::new(sub)).await
    }

    /// Common init path shared by [`spawn`](Self::spawn) and
    /// [`spawn_with_transport`](Self::spawn_with_transport). Takes a
    /// boxed transport the caller has already connected (or a
    /// freshly-spawned `Subprocess`) and drains the init line +
    /// initialize `control_request` exactly once.
    #[allow(clippy::too_many_lines)]
    async fn spawn_inner(options: Options, mut sub: Box<dyn Transport>) -> Result<Self, Error> {
        let can_use_tool = options.can_use_tool.clone();
        let mcp_hosts = McpHosts::new(
            options.mcp_servers.clone(),
            options.external_mcp_servers.clone(),
        );
        let hook_registry = options.hooks.mint_registry();
        let hook_payload = hook_registry.to_initialize_payload();
        let hook_callbacks = hook_registry.by_id;
        // Python `_internal/query.py:196-207` conditionally attaches
        // `agents` / `excludeDynamicSections` / `skills` — forge-sdk
        // matches byte-for-byte so the initialize frame looks identical
        // on the wire when the caller doesn't set any of them.
        let agents_payload =
            if options.agents.is_empty() {
                None
            } else {
                Some(serde_json::to_value(&options.agents).map_err(|e| {
                    Error::message_parse(format!("could not encode agents map: {e}"))
                })?)
            };
        // Concrete-list skills populate `initialize.skills`. `"all"` marker
        // travels via `--allowedTools` only and does NOT appear in the
        // initialize payload (matches Python SDK). An empty list is
        // indistinguishable from "unset" so it drops out too.
        let skills_payload: Vec<String> = options
            .skills
            .iter()
            .filter(|s| s.as_str() != "all")
            .cloned()
            .collect();
        let skills_payload = if skills_payload.is_empty() {
            None
        } else {
            Some(skills_payload)
        };
        // Derive `excludeDynamicSections` from either the preset
        // (Python's canonical path — `types.py:43-66`) OR the
        // Rust-ergonomic top-level `Options::exclude_dynamic_sections`
        // shortcut. Preset wins when both are set.
        let exclude_dynamic_sections = match &options.system_prompt {
            Some(crate::options::SystemPromptKind::Preset {
                exclude_dynamic_sections: Some(v),
                ..
            }) => Some(*v),
            _ => options.exclude_dynamic_sections,
        };

        // Build the initialize control_request body. Python SDK keeps the
        // same field-inclusion rules (`_internal/query.py:196-207`) —
        // `hooks` always present (null when empty), `agents` /
        // `excludeDynamicSections` / `skills` only when explicitly set.
        let hooks_field = if hook_payload
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
        {
            serde_json::Value::Null
        } else {
            hook_payload
        };
        let mut init_body = serde_json::Map::new();
        init_body.insert(
            "subtype".into(),
            serde_json::Value::String("initialize".into()),
        );
        init_body.insert("hooks".into(), hooks_field);
        if let Some(a) = agents_payload {
            init_body.insert("agents".into(), a);
        }
        if let Some(flag) = exclude_dynamic_sections {
            init_body.insert(
                "excludeDynamicSections".into(),
                serde_json::Value::Bool(flag),
            );
        }
        if let Some(list) = skills_payload {
            init_body.insert(
                "skills".into(),
                serde_json::Value::Array(list.into_iter().map(Into::into).collect()),
            );
        }
        let init_request_id = crate::request_id::next();
        let init_envelope = serde_json::json!({
            "type": "control_request",
            "request_id": init_request_id,
            "request": serde_json::Value::Object(init_body),
        });
        let mut init_line = serde_json::to_string(&init_envelope)
            .map_err(|e| Error::message_parse(format!("initialize encode: {e}")))?;
        init_line.push('\n');

        // **Send initialize FIRST.** The CLI in stream-json interactive
        // mode is driven entirely by stdin: it will not emit `system/init`
        // until BOTH (a) an `initialize` control_request is received AND
        // (b) a user message arrives. It DOES emit a `control_response` to
        // the initialize right after hooks complete — that gives us the
        // server-info payload (commands, agents, models, account, pid)
        // without needing to drive a conversation yet.
        //
        // So `spawn_inner` only waits for the control_response; the real
        // `session_id` arrives later on the first user message and gets
        // plumbed through `next_event`. Pinned by
        // `crates/forge-conformance/tests/wire_conformance.rs`.
        sub.write_line(&init_line).await?;

        // Build a partial client now so the init loop can dispatch any
        // `control_request`s the CLI interleaves before its initialize
        // response — specifically, in-process MCP servers will receive
        // `mcp_message` bootstrap calls (JSON-RPC `initialize` /
        // `tools/list`) BEFORE the CLI acknowledges our initialize.
        // `handle_control` needs an `&mut Client`, so we construct it
        // here with empty session_id / initialization_result and fill
        // those in once the loop settles.
        let mut client = Self {
            sub,
            session_id: String::new(),
            line_number: 0,
            can_use_tool,
            mcp_hosts,
            hook_callbacks,
            pre_init_messages: VecDeque::new(),
            initialization_result: None,
        };

        let initialization_result = loop {
            client.line_number += 1;
            let line = client
                .sub
                .read_line()
                .await?
                .ok_or_else(|| Error::Connection {
                    reason: "transport closed stdout before initialize control_response".into(),
                })?;
            let value: serde_json::Value =
                serde_json::from_str(&line).map_err(|source| Error::JsonDecode {
                    line: client.line_number,
                    source,
                })?;
            let ty = value
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            match ty {
                "control_response" => {
                    let resp_request_id = value
                        .pointer("/response/request_id")
                        .and_then(serde_json::Value::as_str);
                    if resp_request_id != Some(&init_request_id) {
                        // The SDK has only sent ONE control_request at this
                        // point (the initialize). Any control_response with a
                        // different request_id means the CLI is responding to
                        // something we never sent — wire corruption or a CLI
                        // bug. Hard-fail rather than swallow.
                        return Err(Error::message_parse(format!(
                            "init: control_response request_id mismatch \
                             (expected {init_request_id:?}, got {resp_request_id:?}); \
                             raw line: {}",
                            line.trim_end()
                        )));
                    }
                    let resp_subtype = value
                        .pointer("/response/subtype")
                        .and_then(serde_json::Value::as_str);
                    if resp_subtype == Some("success") {
                        let body = value
                            .pointer("/response/response")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        break match body {
                            serde_json::Value::Null => None,
                            v => Some(v),
                        };
                    }
                    // No `error` string field — surface the full response
                    // payload so a user staring at the failure can debug
                    // what the CLI actually rejected.
                    let err_msg = value
                        .pointer("/response/error")
                        .and_then(serde_json::Value::as_str)
                        .map_or_else(
                            || {
                                format!(
                                    "no `error` string field; full response: {}",
                                    value.pointer("/response").map_or_else(
                                        || "<missing>".to_string(),
                                        ToString::to_string
                                    )
                                )
                            },
                            ToString::to_string,
                        );
                    return Err(Error::message_parse(format!(
                        "initialize failed: {err_msg}"
                    )));
                }
                "control_request" => {
                    // Inbound CLI → SDK request — most commonly an MCP
                    // `mcp_message` bootstrap call. Dispatch through the
                    // normal handler so the SDK replies on the wire and
                    // the CLI can make progress toward our init response.
                    let req: crate::control::ControlRequest = serde_json::from_value(value)
                        .map_err(|e| {
                            Error::message_parse(format!(
                                "line {}: control_request decode: {e}",
                                client.line_number
                            ))
                        })?;
                    client.handle_control(req).await?;
                }
                "control_cancel_request" => {
                    // Nothing in flight during init — Python's
                    // `query.py:274-280` counterpart is also a no-op.
                    tracing::debug!(
                        line_number = client.line_number,
                        "control_cancel_request during init; nothing live to cancel"
                    );
                }
                _ => {
                    let msg = decode_line(&line, client.line_number)?;
                    debug!(
                        line_number = client.line_number,
                        "buffering pre-init frame for caller"
                    );
                    client.pre_init_messages.push_back(msg);
                }
            }
        };
        debug!("client init handshake complete");

        // Extract the session id from buffered pre-init frames when
        // available. Some CLI configurations (and the mock CLI used in
        // unit tests) emit `system/init` BEFORE the initialize
        // control_response — in that case callers reading `session_id()`
        // right after `spawn` returns see the real id immediately. In
        // the production 2.1.117 stream-json flow, init arrives AFTER
        // the initialize control_response + first user message, so
        // session_id stays empty here and `next_event` populates it
        // when the first session-scoped frame drains.
        client.session_id = client
            .pre_init_messages
            .iter()
            .find_map(|m| m.session_id().map(str::to_string))
            .unwrap_or_default();
        // Drop the `system/init` frame so callers of `next_event` see
        // the clean post-init stream. Python SDK consumes init inside
        // `query._fetch_init` and never surfaces it to callers;
        // forge-sdk mirrors that contract.
        client.pre_init_messages.retain(|m| {
            !matches!(
                m,
                Message::System {
                    subtype,
                    ..
                } if subtype == "init"
            )
        });
        client.initialization_result = initialization_result;
        Ok(client)
    }

    /// Cached response from the `initialize` `control_request` — holds
    /// the CLI's server capabilities, available commands, and output
    /// styles. Returns `None` when the CLI didn't attach a body to its
    /// initialize response. Mirrors Python SDK's
    /// `ClaudeSDKClient.get_server_info` (`client.py:541-564`).
    #[must_use]
    pub fn get_server_info(&self) -> Option<&serde_json::Value> {
        self.initialization_result.as_ref()
    }

    /// The session id captured from the init message.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Send a user prompt as a stream-json user turn.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] on pipe write failure.
    pub async fn send_user_message(&mut self, prompt: &str) -> Result<(), Error> {
        let line = crate::transport::codec::encode_user_prompt(prompt, &self.session_id)?;
        self.sub.write_line(&line).await
    }

    /// Read the next stream-json **regular** message from the subprocess.
    ///
    /// Control requests (permission checks, MCP messages, hook callbacks)
    /// are handled transparently: when one arrives, the client dispatches
    /// to the right callback and writes a `control_response` back, then
    /// loops to the next line. Callers only ever see regular `Message`s.
    ///
    /// Returns `Ok(None)` at end-of-stream (subprocess exited).
    ///
    /// # Errors
    ///
    /// - [`Error::JsonDecode`] / [`Error::MessageParse`] per line.
    /// - [`Error::Io`] on pipe read/write failure.
    pub async fn next_event(&mut self) -> Result<Option<Message>, Error> {
        // Drain any messages we buffered during spawn_inner before reading
        // fresh lines from the transport.
        if let Some(buffered) = self.pre_init_messages.pop_front() {
            self.capture_session_id_from(&buffered);
            return Ok(Some(buffered));
        }
        loop {
            let Some(line) = self.sub.read_line().await? else {
                return Ok(None);
            };
            self.line_number += 1;
            match decode_dispatch(&line, self.line_number)? {
                DecodedLine::Message(msg) => {
                    self.capture_session_id_from(&msg);
                    return Ok(Some(msg));
                }
                DecodedLine::Control(req) => {
                    self.handle_control(req).await?;
                }
                DecodedLine::ControlCancel { request_id } => {
                    // Python SDK (`_internal/query.py:274-280`) cancels the
                    // in-flight control handler tied to `request_id`.
                    // forge-sdk dispatches control handlers synchronously on
                    // the read loop, so by the time we see the cancel frame
                    // the handler has already completed — there is nothing
                    // live to cancel. Log and drop, keeping the loop alive.
                    tracing::debug!(%request_id, "control_cancel_request received; nothing to cancel");
                }
                DecodedLine::ControlResponse { request_id, .. } => {
                    // A control_response only reaches `next_event` if it
                    // arrives AFTER `send_control`'s synchronous wait has
                    // already returned — either the CLI double-responded,
                    // or the request_id drifted. Neither is expected in
                    // a well-behaved session; log + skip rather than crash.
                    tracing::warn!(
                        %request_id,
                        "unexpected control_response outside send_control loop — dropping"
                    );
                }
                DecodedLine::Unknown { type_str, raw } => {
                    // Forward-compat: the CLI emitted a frame with a `type`
                    // we don't recognise. Log loudly so anyone watching
                    // logs notices drift, then skip — keeping the read
                    // loop alive rather than panicking the whole session.
                    tracing::warn!(
                        type = %type_str,
                        raw = %raw,
                        line = self.line_number,
                        "unknown top-level stream-json type — skipping (harness should flag)"
                    );
                }
            }
        }
    }

    /// Populate `self.session_id` from the first message that carries
    /// one. No-op once we've cached a non-empty value. Called on every
    /// message observed through `next_event`.
    fn capture_session_id_from(&mut self, msg: &Message) {
        if self.session_id.is_empty() {
            if let Some(id) = msg.session_id() {
                if !id.is_empty() {
                    self.session_id = id.to_string();
                    debug!(session_id = %self.session_id, "client session_id bound");
                }
            }
        }
    }

    /// Drain messages until and including a [`Message::Result`]. The
    /// result frame IS included in the returned vector. Convenience over
    /// [`next_event`](Self::next_event) for one-shot request/response
    /// workflows where callers don't want to write their own termination
    /// loop. Mirrors Python SDK's `ClaudeSDKClient.receive_response`
    /// (`client.py:566-605`).
    ///
    /// Returns whatever was received before the subprocess closed if no
    /// `Result` frame ever arrives.
    ///
    /// # Errors
    ///
    /// Any error [`next_event`](Self::next_event) surfaces.
    pub async fn receive_response(&mut self) -> Result<Vec<Message>, Error> {
        let mut msgs = Vec::new();
        while let Some(msg) = self.next_event().await? {
            let is_result = matches!(msg, Message::Result { .. });
            msgs.push(msg);
            if is_result {
                break;
            }
        }
        Ok(msgs)
    }

    /// Graceful shutdown. Closes stdin, waits for the subprocess to exit.
    ///
    /// # Errors
    ///
    /// [`Error::Process`] when the subprocess exits non-zero, [`Error::Io`]
    /// for I/O failure.
    pub async fn disconnect(mut self) -> Result<(), Error> {
        self.sub.close().await
    }

    /// Close the stdin side of the transport while keeping the client
    /// alive for reading. Signals the CLI that no more user messages
    /// are coming; the CLI drains in-flight turns, emits any final
    /// frames, and closes stdout. `next_event` returns `Ok(None)` on
    /// EOF afterwards.
    ///
    /// Useful for scenarios that want to drain the conversation to
    /// completion without prematurely killing the subprocess.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] on write failure.
    pub async fn end_input(&mut self) -> Result<(), Error> {
        self.sub.end_input().await
    }

    /// Spawn a client around a caller-supplied [`Transport`]
    /// implementation, bypassing the internal `Subprocess` construction.
    ///
    /// Useful for testing with an in-memory transport, or for hosting
    /// the `claude` binary in a non-local environment (remote SSH,
    /// containerised sandbox, etc.). The passed transport MUST be
    /// ready to serve I/O — this constructor sends an `initialize`
    /// `control_request` first, then drains the response (interleaved
    /// with any `control_request`s the CLI sends in the meantime,
    /// e.g. MCP `mcp_message` bootstrap calls). Any `system/init`
    /// frame the CLI emits is captured along the way and surfaced
    /// via [`session_id()`](Self::session_id) / [`next_event`](Self::next_event).
    /// Mock `Transport` implementations should respond to the
    /// outbound `initialize` request — they do NOT need to emit
    /// `system/init` first (the real CLI gates that frame on a
    /// later user message; matching that ordering avoids deadlocks).
    ///
    /// All initialisation logic (MCP host wiring, hook registry) runs
    /// identically to [`spawn`](Self::spawn) — only the I/O
    /// implementation differs.
    ///
    /// # Errors
    ///
    /// Any [`Error`] variant — see [`Client::spawn`].
    pub async fn spawn_with_transport(
        options: Options,
        transport: Box<dyn Transport>,
    ) -> Result<Self, Error> {
        Self::spawn_inner(options, transport).await
    }
}
