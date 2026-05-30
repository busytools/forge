//! Public [`Client`]  -  the entry point consumers hold.
//!
//! `Client` is a cheap-clone handle (`Arc`-backed) over a background
//! reader task that owns the `claude` subprocess. All methods take
//! `&self`, so the same `Client` can be passed by reference into
//! multiple tasks. The events stream is returned alongside the Client
//! at spawn time as a [`ClientEvents`] receiver  -  single-consumer,
//! independent of the writer-side handle.
//!
//! Internally:
//! - The reader task pulls lines from the subprocess, decodes them,
//!   pushes regular [`Message`]s onto an mpsc that backs the returned
//!   [`ClientEvents`], dispatches inbound `control_request`s on
//!   detached tasks, and routes outbound `control_response`s to
//!   per-request oneshots so [`Client::send_control`] callers can
//!   `await` their typed reply.
//! - The writer half is a clonable
//!   [`SharedWriter`](crate::transport::process::SharedWriter) cloned
//!   from [`Subprocess`](crate::transport::process::Subprocess); outbound
//!   writes go through it without contending on `&mut self`.

pub(crate) mod control_dispatch;
mod control_send;
pub(crate) mod runtime;

pub(crate) use control_dispatch::ControlDispatchHandle;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::Error;
use crate::client::runtime::{
    PendingControls, SharedSessionId, new_shared_session_id, spawn_reader_task,
};
use crate::mcp::orchestration::McpHosts;
use crate::options::Options;
use crate::transport::codec::{DecodedLine, decode_dispatch};
use crate::transport::process::Subprocess;
use forge_primitives::Message;

/// Stream of regular [`Message`]s produced by the reader task.
/// Returned alongside [`Client`] from [`Client::spawn`]. Single-consumer;
/// the writer-side [`Client`] is `Clone` and can move freely between
/// tasks while one task owns the receiver.
pub type ClientEvents = mpsc::UnboundedReceiver<Result<Message, Error>>;

/// An active `claude` binary subprocess.
///
/// Construct via [`spawn`](Self::spawn). The init handshake
/// (`initialize` `control_request` + drained response) runs inside
/// `spawn`; the `system/init` frame is consumed and its session id
/// cached on [`session_id()`](Self::session_id). The returned
/// [`ClientEvents`] receiver yields the clean conversational stream
/// starting with hook/assistant/user frames.
///
/// `Client` is `Clone`. All clones share the same underlying
/// subprocess, reader task, and pending-control map. Cloning is
/// cheap (an `Arc` increment); pass `Client` by value or reference
/// freely.
#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    /// Cloned writer (mpsc-backed via the transport's writer task).
    writer: std::sync::Arc<crate::transport::process::SharedWriter>,
    /// PID of the spawned `claude` child captured at spawn time.
    /// Stable for the lifetime of the subprocess; consumers (e.g. the
    /// Inspector pane's OS-level process walker) cache snapshots
    /// keyed off this value. `None` when the transport couldn't
    /// report a PID (in practice never for a live subprocess, but
    /// the optional makes the test stubs and post-disconnect path
    /// total).
    claude_pid: Option<u32>,
    /// Cached response from the `initialize` `control_request`  -
    /// populated during spawn and never mutated afterwards.
    initialization_result: Option<serde_json::Value>,
    /// Captured `system/init` payload (`model`, `tools`, `mcp_servers`,
    /// `slash_commands`, …). The CLI emits this once after init and the
    /// SDK strips it from the user-visible `Message` stream; cache it
    /// here so `forge-agent` can answer footer / slash queries without
    /// re-running the handshake.
    cached_init_data: Option<serde_json::Value>,
    /// Captured session id. The reader task updates it as messages
    /// arrive; consumers read the current value via
    /// [`Client::session_id`].
    session_id: SharedSessionId,
    /// In-flight outbound `control_request`s waiting on responses.
    /// The reader task routes incoming `control_response`s here.
    pending_controls: PendingControls,
    /// Reader task handle. Awaited on [`Client::disconnect`].
    reader_task: Mutex<Option<JoinHandle<()>>>,
    /// Shutdown signal for the reader task. `take()`'d on the first
    /// [`Client::disconnect`] call; subsequent calls are no-ops.
    shutdown_tx: Mutex<Option<oneshot::Sender<()>>>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("session_id", &"<shared>")
            .field(
                "initialization_result",
                &self.inner.initialization_result.as_ref().map(|_| "<cached>"),
            )
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Spawn `claude` with the given options and run the init handshake.
    ///
    /// Wire-recording for conformance baselines is configured via
    /// [`Options::tee_inbound`](crate::Options) /
    /// [`Options::tee_outbound`](crate::Options) callbacks  -  the
    /// `forge-test-harness` crate drives this.
    ///
    /// # Errors
    ///
    /// Any [`Error`] variant; see field docs.
    pub async fn spawn(options: Options) -> Result<(Self, ClientEvents), Error> {
        let mut sub = Subprocess::spawn(&options).await?;
        let writer = sub.clone_writer();
        // Capture the PID before the reader task consumes `sub`. It
        // doesn't change for the subprocess's lifetime so caching
        // once is correct.
        let claude_pid = sub.child_pid();
        let session_id = new_shared_session_id();
        let mcp_hosts =
            McpHosts::new(options.mcp_servers.clone(), options.external_mcp_servers.clone());
        let hook_registry = options.hooks.mint_registry();
        let hook_payload = hook_registry.to_initialize_payload();
        let hook_callbacks = hook_registry.by_id;

        // Build the dispatch handle now so the init loop can route
        // interleaved `control_request`s through it.
        let dispatch = ControlDispatchHandle::new(
            writer.clone(),
            options.can_use_tool.clone(),
            options.auto_approve_tool.clone(),
            mcp_hosts,
            hook_callbacks,
            session_id.clone(),
        );

        // Build the initialize control_request body. the CLI keeps
        // the same field-inclusion rules:
        // `hooks` always present (null when empty), `agents` /
        // `excludeDynamicSections` / `skills` only when explicitly set.
        let agents_payload = if options.subagents.is_empty() {
            None
        } else {
            Some(
                serde_json::to_value(&options.subagents)
                    .map_err(|e| Error::encode("subagents map", e))?,
            )
        };
        let exclude_dynamic_sections = match &options.system_prompt {
            Some(crate::options::SystemPromptKind::Preset {
                exclude_dynamic_sections: Some(v),
                ..
            }) => Some(*v),
            _ => None,
        };
        let hooks_field = if hook_payload.as_object().is_some_and(serde_json::Map::is_empty) {
            serde_json::Value::Null
        } else {
            hook_payload
        };
        let mut init_body = serde_json::Map::new();
        init_body.insert("subtype".into(), serde_json::Value::String("initialize".into()));
        init_body.insert("hooks".into(), hooks_field);
        if let Some(a) = agents_payload {
            init_body.insert("agents".into(), a);
        }
        if let Some(flag) = exclude_dynamic_sections {
            init_body.insert("excludeDynamicSections".into(), serde_json::Value::Bool(flag));
        }
        let init_request_id = crate::request_id::next();
        let init_envelope = serde_json::json!({
            "type": "control_request",
            "request_id": init_request_id,
            "request": serde_json::Value::Object(init_body),
        });
        let mut init_line = serde_json::to_string(&init_envelope)
            .map_err(|e| Error::encode("initialize body", e))?;
        init_line.push('\n');

        // **Send initialize FIRST.** The CLI in stream-json interactive
        // mode is driven entirely by stdin: it will not emit `system/init`
        // until BOTH (a) an `initialize` control_request is received AND
        // (b) a user message arrives. It DOES emit a `control_response` to
        // the initialize right after hooks complete  -  that gives us the
        // server-info payload (commands, agents, models, account, pid)
        // without needing to drive a conversation yet.
        sub.write_line(&init_line).await?;

        let mut pre_init_messages: VecDeque<Message> = VecDeque::new();
        let mut cached_init_data: Option<serde_json::Value> = None;
        let mut line_number: u64 = 0;
        let initialization_result = loop {
            line_number += 1;
            let line = sub.read_line().await?.ok_or_else(|| Error::Connection {
                reason: "transport closed stdout before initialize control_response".into(),
            })?;
            let value: serde_json::Value = serde_json::from_str(&line)
                .map_err(|source| Error::JsonDecode { line: line_number, source })?;
            let ty = value.get("type").and_then(serde_json::Value::as_str).unwrap_or("");
            match ty {
                "control_response" => {
                    let resp_request_id =
                        value.pointer("/response/request_id").and_then(serde_json::Value::as_str);
                    if resp_request_id != Some(&init_request_id) {
                        return Err(Error::message_parse(format!(
                            "init: control_response request_id mismatch \
                             (expected {init_request_id:?}, got {resp_request_id:?}); \
                             raw line: {}",
                            line.trim_end()
                        )));
                    }
                    let resp_subtype =
                        value.pointer("/response/subtype").and_then(serde_json::Value::as_str);
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
                    let err_msg = value
                        .pointer("/response/error")
                        .and_then(serde_json::Value::as_str)
                        .map_or_else(
                            || {
                                format!(
                                    "no `error` string field; full response: {}",
                                    value.pointer("/response").map_or_else(
                                        || "<missing>".to_string(),
                                        ToString::to_string,
                                    )
                                )
                            },
                            ToString::to_string,
                        );
                    return Err(Error::message_parse(format!("initialize failed: {err_msg}")));
                }
                "control_request" => {
                    // Interleaved CLI → SDK request during init  -
                    // most commonly an MCP `mcp_message` bootstrap.
                    // Dispatch synchronously through the dispatch
                    // handle (the reader task isn't running yet, so
                    // there's no concurrent reader to worry about).
                    let req: crate::control::ControlRequest = serde_json::from_value(value)
                        .map_err(|e| {
                            Error::message_parse(format!(
                                "line {line_number}: control_request decode: {e}"
                            ))
                        })?;
                    dispatch.dispatch(req).await?;
                }
                "control_cancel_request" => {
                    tracing::debug!(
                        line_number,
                        "control_cancel_request during init; nothing live to cancel"
                    );
                }
                _ => match decode_dispatch(&line, line_number)? {
                    DecodedLine::Message(msg) => {
                        debug!(line_number, "buffering pre-init frame for caller");
                        // Capture session id off pre-init messages so
                        // callers reading session_id() right after
                        // spawn see a real value when the CLI happens
                        // to emit init early.
                        if let Some(id) = msg.session_id()
                            && !id.is_empty()
                        {
                            let mut current = session_id.write();
                            if current.is_empty() {
                                *current = id.to_string();
                            }
                        }
                        // Drop `system/init` from the pre-init buffer  -
                        // the CLI consumes it inside `query._fetch_init`
                        // and never surfaces it to callers; we mirror.
                        // Cache its `data` so `forge-agent` can read
                        // model / mcp / slash-command info off it.
                        if let Message::System { ref subtype, ref data, .. } = msg
                            && subtype == "init"
                        {
                            cached_init_data = Some(data.clone());
                        } else {
                            pre_init_messages.push_back(msg);
                        }
                    }
                    DecodedLine::Unknown { type_str, raw } => {
                        tracing::warn!(
                            type = %type_str,
                            raw = %raw,
                            line = line_number,
                            "unknown top-level type during init  -  buffering as Message::Unknown"
                        );
                        pre_init_messages.push_back(Message::Unknown { type_str, raw });
                    }
                    other => {
                        debug!(
                            line_number,
                            ?other,
                            "unexpected DecodedLine during init fallthrough; ignoring"
                        );
                    }
                },
            }
        };
        debug!("client init handshake complete");

        let pending_controls: PendingControls = Arc::new(Mutex::new(HashMap::new()));
        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let reader_task = spawn_reader_task(
            sub,
            dispatch,
            pending_controls.clone(),
            events_tx,
            pre_init_messages.into_iter().collect(),
            shutdown_rx,
        );

        let inner = Arc::new(ClientInner {
            writer,
            claude_pid,
            initialization_result,
            cached_init_data,
            session_id,
            pending_controls,
            reader_task: Mutex::new(Some(reader_task)),
            shutdown_tx: Mutex::new(Some(shutdown_tx)),
        });

        Ok((Self { inner }, events_rx))
    }

    /// Cached response from the `initialize` `control_request`  -  holds
    /// the CLI's server capabilities, available commands, and output
    /// styles. Returns `None` when the CLI didn't attach a body to its
    /// initialize response.
    pub fn get_server_info(&self) -> Option<&serde_json::Value> {
        self.inner.initialization_result.as_ref()
    }

    /// OS-level PID of the spawned `claude` child, captured at spawn
    /// time. Stable for the lifetime of this `Client`. Used by
    /// downstream consumers (e.g. `forge_agent::env::processes`) to
    /// walk the child process tree for the Inspector pane's
    /// PROCESSES section.
    ///
    /// Returns `None` only when the transport couldn't report a PID
    /// at spawn  -  never happens for a real subprocess on supported
    /// platforms, but the `Option` keeps the test-stub path
    /// expressible.
    pub fn claude_pid(&self) -> Option<u32> {
        self.inner.claude_pid
    }

    /// Captured `system/init` payload  -  the CLI's first session-scoped
    /// frame, carrying `model`, `tools`, `mcp_servers`, `slash_commands`,
    /// `agents`, `skills`, etc. Stripped from the user-visible `Message`
    /// stream during init; cached here for callers that need the
    /// initial session context (e.g. forge-agent's status snapshot +
    /// slash autocomplete).
    pub fn initial_session_data(&self) -> Option<&serde_json::Value> {
        self.inner.cached_init_data.as_ref()
    }

    /// The session id captured from the init message. Returns an empty
    /// string until the first session-scoped frame arrives.
    pub fn session_id(&self) -> String {
        self.inner.session_id.read().clone()
    }

    /// Read the bare `apiKeySource` field from the cached
    /// `system/init` payload, returning a partial
    /// [`forge_primitives::AccountInfo`] (auth source only  -  no
    /// email/org/subscription). Returns `None` when the payload is
    /// absent (init not yet arrived), the field is missing, or the
    /// value is empty / `"none"`.
    ///
    /// Callers that need the full profile (email, organization,
    /// subscription tier) shell out to `claude auth status`
    /// separately  -  that path is agent-side
    /// (`forge_agent::cloud::auth_status`), not SDK-side, because it
    /// spawns a fresh subprocess outside the long-lived stream-json
    /// session.
    pub fn account_info_from_init(&self) -> Option<forge_primitives::AccountInfo> {
        let data = self.inner.cached_init_data.as_ref()?;
        let api_key_source = data
            .get("apiKeySource")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .filter(|s| !s.is_empty() && s != "none")?;
        Some(forge_primitives::AccountInfo {
            api_key_source: Some(api_key_source.clone()),
            token_source: Some(api_key_source),
            ..forge_primitives::AccountInfo::default()
        })
    }

    /// Send a user prompt as a stream-json user turn.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] on pipe write failure.
    pub async fn send_user_message(&self, prompt: &str) -> Result<(), Error> {
        let session_id = self.session_id();
        let line = crate::transport::codec::encode_user_prompt(prompt, &session_id)?;
        self.inner.writer.write_line(&line).await
    }

    /// Send a user prompt with structured content blocks (text +
    /// image, etc.) as a stream-json user turn. Use this when the
    /// prompt is multi-modal; for plain text use
    /// [`send_user_message`](Self::send_user_message).
    ///
    /// `content` is forwarded verbatim as the message body's
    /// `content` field  -  callers must build CLI-compatible block
    /// objects (e.g. `{"type":"text","text":"..."}`,
    /// `{"type":"image","source":{"type":"base64","media_type":"image/png","data":"..."}}`).
    ///
    /// # Errors
    ///
    /// [`Error::Io`] on pipe write failure;
    /// [`Error::MessageParse`] on JSON serialization failure.
    pub async fn send_user_message_with_content(
        &self,
        content: &[serde_json::Value],
    ) -> Result<(), Error> {
        let session_id = self.session_id();
        let line = crate::transport::codec::encode_user_prompt_with_content(content, &session_id)?;
        self.inner.writer.write_line(&line).await
    }

    /// Close the stdin side of the transport while keeping the client
    /// alive for reading. Signals the CLI that no more user messages
    /// are coming; the CLI drains in-flight turns, emits any final
    /// frames, and closes stdout. The events receiver returns `None`
    /// on EOF afterwards.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] on write failure.
    pub async fn end_input(&self) -> Result<(), Error> {
        self.inner.writer.end_input().await
    }

    /// Graceful shutdown. Signals the reader task to stop, waits for
    /// it to drain and close the subprocess. Idempotent  -  subsequent
    /// calls (on this clone or any other) are no-ops.
    ///
    /// # Errors
    ///
    /// None today; the inner reader-task close is best-effort. Kept
    /// `Result` for forward-compat.
    pub async fn disconnect(&self) -> Result<(), Error> {
        if let Some(tx) = self.inner.shutdown_tx.lock().await.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.inner.reader_task.lock().await.take()
            && let Err(join_err) = handle.await
        {
            tracing::error!(
                target: "forge_sdk",
                error = %join_err,
                "reader task panicked during disconnect"
            );
        }
        Ok(())
    }

    /// Internal: outbound `control_request` issuer. Inserts a oneshot
    /// into `pending_controls`, writes the envelope via the cloned
    /// writer, and awaits the reader task's routed response.
    pub(crate) async fn send_control(
        &self,
        subtype: &str,
        extra: serde_json::Value,
    ) -> Result<serde_json::Value, Error> {
        let request_id = crate::request_id::next();
        let mut request_body = serde_json::Map::new();
        request_body.insert("subtype".into(), serde_json::Value::String(subtype.to_string()));
        if let serde_json::Value::Object(extra_map) = extra {
            for (k, v) in extra_map {
                request_body.insert(k, v);
            }
        }
        let envelope = serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": serde_json::Value::Object(request_body),
        });
        let mut line = serde_json::to_string(&envelope)
            .map_err(|e| Error::encode("control_request envelope", e))?;
        line.push('\n');

        let (resp_tx, resp_rx) = oneshot::channel();
        {
            let mut pending = self.inner.pending_controls.lock().await;
            pending.insert(request_id.clone(), resp_tx);
        }
        if let Err(e) = self.inner.writer.write_line(&line).await {
            // Remove the pending entry on write failure so the EOF
            // drain doesn't fire on a request that never went out.
            let mut pending = self.inner.pending_controls.lock().await;
            pending.remove(&request_id);
            return Err(e);
        }
        match resp_rx.await {
            Ok(outcome) => outcome,
            Err(_) => Err(Error::Connection {
                reason: format!("subprocess closed before {subtype} response"),
            }),
        }
    }
}
