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

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::debug;

use crate::Error;
use crate::hooks::ErasedHookCallback;
use crate::mcp::orchestration::McpHosts;
use crate::messages::Message;
use crate::options::Options;
use crate::permissions::CanUseToolCallback;
use crate::transcript_mirror_batcher::TranscriptMirrorBatcher;
use crate::transport::codec::{DecodedLine, decode_dispatch, decode_line};
use crate::transport::process::Subprocess;

/// An active `claude` binary subprocess.
///
/// Construct via [`spawn`](Self::spawn). The first line the binary emits is
/// always a `system`/`init` message carrying the session id — `spawn`
/// consumes it so callers start clean at the first `assistant` turn.
pub struct Client {
    pub(crate) sub: Subprocess,
    pub(crate) session_id: String,
    pub(crate) line_number: u64,
    pub(crate) can_use_tool: Option<Arc<dyn CanUseToolCallback>>,
    pub(crate) mcp_hosts: McpHosts,
    pub(crate) hook_callbacks: HashMap<String, Arc<dyn ErasedHookCallback>>,
    pub(crate) mirror_batcher: Option<TranscriptMirrorBatcher>,
    pub(crate) synth_messages: Option<mpsc::UnboundedReceiver<Message>>,
    /// Response payload from the `initialize` `control_request`, cached so
    /// [`get_server_info`](Self::get_server_info) can return it without
    /// re-issuing the handshake. Python stores the same payload at
    /// `_internal/query.py:214` (`self._initialization_result`).
    pub(crate) initialization_result: Option<serde_json::Value>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("sub", &self.sub)
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
                "mirror_batcher",
                &self.mirror_batcher.as_ref().map(|_| "<batcher>"),
            )
            .field(
                "synth_messages",
                &self.synth_messages.as_ref().map(|_| "<rx>"),
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
    #[allow(clippy::too_many_lines)]
    pub async fn spawn(options: Options) -> Result<Self, Error> {
        // Pre-flight validation — fail fast on misconfigured combos
        // so the error lands at spawn time rather than mid-session.
        // Mirrors Python `_internal/session_store_validation.py:40-45`:
        // a session_store handles the on-disk mirror; file checkpoints
        // are local-disk-only and would diverge from the mirrored
        // transcript if both were on at once.
        if let Some(store) = &options.session_store {
            if options.enable_file_checkpointing {
                return Err(Error::message_parse(
                    "session_store cannot be combined with enable_file_checkpointing \
                     (checkpoints are local-disk only and would diverge from the \
                     mirrored transcript)",
                ));
            }
            // Mirrors Python `_internal/session_store_validation.py:28-38`.
            // When `resume` is set, `list_sessions` is never called; a
            // minimal store without it is fine. Otherwise require the
            // impl to override the default NotImplemented.
            if options.continue_conversation
                && options.resume.is_none()
                && !store.provides_list_sessions()
            {
                return Err(Error::message_parse(
                    "continue_conversation with session_store requires the store to \
                     implement list_sessions()",
                ));
            }
        }

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

        let mut sub = Subprocess::spawn(&options).await?;
        let init_line = sub.read_line().await?.ok_or_else(|| Error::Connection {
            reason: "subprocess closed stdout before init line".into(),
        })?;
        let init = decode_line(&init_line, 1)?;
        let session_id = match &init {
            Message::System {
                session_id: Some(id),
                subtype,
                ..
            } if subtype == "init" => id.clone(),
            other => {
                return Err(Error::message_parse(format!(
                    "expected system/init, got: {other:?}"
                )));
            }
        };
        debug!(session_id, "client init");

        let (mirror_batcher, synth_messages) = if let Some(store) = options.session_store.clone() {
            let projects_dir_str = options.projects_dir.as_ref().map_or_else(
                || {
                    crate::session::mutations::projects_dir()
                        .to_string_lossy()
                        .into_owned()
                },
                |p| p.to_string_lossy().into_owned(),
            );
            let (tx, rx) = mpsc::unbounded_channel();
            (
                Some(TranscriptMirrorBatcher::new(store, projects_dir_str, tx)),
                Some(rx),
            )
        } else {
            (None, None)
        };

        let mut client = Self {
            sub,
            session_id,
            line_number: 1,
            can_use_tool,
            mcp_hosts,
            hook_callbacks,
            mirror_batcher,
            synth_messages,
            initialization_result: None,
        };
        client
            .send_initialize(
                hook_payload,
                skills_payload,
                exclude_dynamic_sections,
                agents_payload,
            )
            .await?;
        Ok(client)
    }

    /// Send the `initialize` `control_request` and await its matching
    /// `control_response`. Python SDK does this right after receiving the
    /// system/init message, before any user input. The decoded response
    /// body is cached on [`initialization_result`](Self::initialization_result)
    /// so [`get_server_info`](Self::get_server_info) can surface it later
    /// without a second round-trip (matches Python
    /// `_internal/query.py:214`).
    ///
    /// Field-inclusion matches Python `_internal/query.py:196-207`:
    /// - `hooks` — always present; value is `null` when no callbacks
    ///   are registered.
    /// - `agents` — only when the caller has agents configured.
    /// - `excludeDynamicSections` — only when the caller set a value.
    /// - `skills` — only when the caller supplied a concrete list (the
    ///   `"all"` sentinel travels via `--allowedTools` only).
    async fn send_initialize(
        &mut self,
        hooks: serde_json::Value,
        skills: Option<Vec<String>>,
        exclude_dynamic_sections: Option<bool>,
        agents: Option<serde_json::Value>,
    ) -> Result<(), Error> {
        let hooks_field = if hooks.as_object().is_some_and(serde_json::Map::is_empty) {
            serde_json::Value::Null
        } else {
            hooks
        };
        let mut body = serde_json::Map::new();
        body.insert("hooks".into(), hooks_field);
        if let Some(a) = agents {
            body.insert("agents".into(), a);
        }
        if let Some(flag) = exclude_dynamic_sections {
            body.insert(
                "excludeDynamicSections".into(),
                serde_json::Value::Bool(flag),
            );
        }
        if let Some(list) = skills {
            body.insert(
                "skills".into(),
                serde_json::Value::Array(list.into_iter().map(Into::into).collect()),
            );
        }
        let response = self
            .send_control("initialize", serde_json::Value::Object(body))
            .await?;
        // `send_control` returns `Value::Null` when the CLI replies with
        // an empty success body; store `None` in that case so
        // `get_server_info()` reflects "no info" cleanly.
        self.initialization_result = if response.is_null() {
            None
        } else {
            Some(response)
        };
        Ok(())
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
        loop {
            // Drain any synthesised messages (e.g. MirrorError from the
            // transcript-mirror batcher) before blocking on the subprocess.
            if let Some(rx) = self.synth_messages.as_mut() {
                if let Ok(msg) = rx.try_recv() {
                    return Ok(Some(msg));
                }
            }
            let Some(line) = self.sub.read_line().await? else {
                // Final flush before stream end — matches Python SDK's
                // teardown hook in `_internal/query.py`.
                self.flush_mirror().await;
                // After flush, the batcher may have pushed a MirrorError.
                if let Some(rx) = self.synth_messages.as_mut() {
                    if let Ok(msg) = rx.try_recv() {
                        return Ok(Some(msg));
                    }
                }
                return Ok(None);
            };
            self.line_number += 1;
            match decode_dispatch(&line, self.line_number)? {
                DecodedLine::Message(msg) => {
                    // Flush mirror on result frames — Python SDK behaviour at
                    // `_internal/query.py:292-297`.
                    if let Message::Result { .. } = &msg {
                        self.flush_mirror().await;
                    }
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
                DecodedLine::TranscriptMirror { file_path, entries } => {
                    self.handle_transcript_mirror(file_path, entries);
                }
            }
        }
    }

    /// Handle a top-level `{"type":"transcript_mirror","filePath":...,
    /// "entries":[...]}` frame. Enqueues into the batcher when a session
    /// store is configured; otherwise drops.
    ///
    /// Wire shape verified against Python v0.1.64
    /// `_internal/transcript_mirror_batcher.py` + `_internal/query.py:282-289`.
    /// Coalescing, eager-flush thresholds, and `on_error`-synthesised
    /// `MirrorError` frames live in
    /// [`TranscriptMirrorBatcher`](crate::transcript_mirror_batcher).
    fn handle_transcript_mirror(
        &self,
        file_path: String,
        entries: Vec<crate::session::store::SessionStoreEntry>,
    ) {
        if entries.is_empty() {
            return;
        }
        let Some(batcher) = self.mirror_batcher.as_ref() else {
            return;
        };
        batcher.enqueue(file_path, entries);
    }

    /// Flush the transcript-mirror batcher — called on every `result` frame
    /// and at stream-end / disconnect.
    async fn flush_mirror(&self) {
        if let Some(batcher) = self.mirror_batcher.as_ref() {
            batcher.flush().await;
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
    pub async fn disconnect(self) -> Result<(), Error> {
        if let Some(batcher) = &self.mirror_batcher {
            batcher.close().await;
        }
        self.sub.shutdown().await
    }
}
