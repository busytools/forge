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
        let can_use_tool = options.can_use_tool.clone();
        let mcp_hosts = McpHosts::new(
            options.mcp_servers.clone(),
            options.external_mcp_servers.clone(),
        );
        let hook_registry = options.hooks.mint_registry();
        let hook_payload = hook_registry.to_initialize_payload();
        let hook_callbacks = hook_registry.by_id;
        let agents_payload = if options.agents.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::to_value(&options.agents).map_err(|e| Error::MessageParse {
                reason: format!("could not encode agents map: {e}"),
            })?
        };
        // Concrete-list skills populate `initialize.skills`. `"all"` marker
        // travels via `--allowedTools` only and does NOT appear in the
        // initialize payload (matches Python SDK).
        let skills_payload: Vec<String> = options
            .skills
            .iter()
            .filter(|s| s.as_str() != "all")
            .cloned()
            .collect();
        let exclude_dynamic_sections = options.exclude_dynamic_sections;

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
                return Err(Error::MessageParse {
                    reason: format!("expected system/init, got: {other:?}"),
                });
            }
        };
        debug!(session_id, "client init");

        let (mirror_batcher, synth_messages) = if let Some(store) = options.session_store.clone() {
            let projects_dir_str = options.projects_dir.as_ref().map_or_else(
                || {
                    crate::session_mutations::projects_dir()
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
    /// system/init message, before any user input.
    ///
    /// The body carries:
    /// - `hooks` — `{event_name: [{matcher, hookCallbackIds, timeout}]}`
    /// - `excludeDynamicSections` — bool
    /// - `skills` — list of concrete skill names (the `"all"` sentinel
    ///   travels via `--allowedTools` only).
    async fn send_initialize(
        &mut self,
        hooks: serde_json::Value,
        skills: Vec<String>,
        exclude_dynamic_sections: bool,
        agents: serde_json::Value,
    ) -> Result<(), Error> {
        self.send_control(
            "initialize",
            serde_json::json!({
                "hooks": hooks,
                "excludeDynamicSections": exclude_dynamic_sections,
                "skills": skills,
                "agents": agents,
            }),
        )
        .await?;
        Ok(())
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
        entries: Vec<crate::session_store::SessionStoreEntry>,
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
