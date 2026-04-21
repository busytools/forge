//! Public [`Client`] — the entry point consumers hold.

use std::sync::Arc;

use tracing::debug;

use crate::Error;
use crate::control::{AllowBehavior, ControlRequest, ControlRequestKind};
use crate::messages::Message;
use crate::options::Options;
use crate::permissions::{CanUseToolCallback, PermissionDecision, ToolPermissionContext};
use crate::transport::codec::{DecodedLine, decode_dispatch, decode_line, encode_user_prompt};
use crate::transport::process::Subprocess;

/// An active `claude` binary subprocess.
///
/// Construct via [`spawn`](Self::spawn). The first line the binary emits is
/// always a `system`/`init` message carrying the session id — `spawn`
/// consumes it so callers start clean at the first `assistant` turn.
pub struct Client {
    sub: Subprocess,
    session_id: String,
    line_number: u64,
    can_use_tool: Option<Arc<dyn CanUseToolCallback>>,
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
        Ok(Self {
            sub,
            session_id,
            line_number: 1,
            can_use_tool,
        })
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
        let line = encode_user_prompt(prompt, &self.session_id)?;
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
            let Some(line) = self.sub.read_line().await? else {
                return Ok(None);
            };
            self.line_number += 1;
            match decode_dispatch(&line, self.line_number)? {
                DecodedLine::Message(msg) => return Ok(Some(msg)),
                DecodedLine::Control(req) => {
                    self.handle_control(req).await?;
                }
            }
        }
    }

    async fn handle_control(&mut self, req: ControlRequest) -> Result<(), Error> {
        // Capture the original input here — we need it to echo into
        // `updatedInput` when the callback supplies no override.
        let original_input = req.original_tool_input().cloned();

        let decision = match (&self.can_use_tool, &req.request) {
            (
                Some(cb),
                ControlRequestKind::CanUseTool {
                    tool_name,
                    input,
                    tool_use_id,
                    agent_id,
                    ..
                },
            ) => {
                let ctx = ToolPermissionContext::new(
                    tool_name.clone(),
                    input.clone(),
                    tool_use_id.clone(),
                    agent_id.clone(),
                );
                cb.call(ctx).await
            }
            (None, ControlRequestKind::CanUseTool { .. }) => {
                // No callback registered — default to deny. This matches
                // what the binary would see if the SDK had no business
                // being in the loop.
                PermissionDecision::deny("no permission callback registered")
            }
            _ => {
                // McpMessage / HookCallback / other subtypes handled elsewhere
                // (see M3 MCP routing, Plan 3 hooks). Until they're wired up,
                // respond with an unsupported error.
                return self.write_unsupported_control_error(&req).await;
            }
        };

        let behavior = if decision.is_allow() {
            AllowBehavior::Allow {
                updated_input: decision
                    .updated_input()
                    .cloned()
                    .or(original_input)
                    .unwrap_or(serde_json::Value::Null),
                updated_permissions: None,
            }
        } else {
            AllowBehavior::Deny {
                message: decision.reason().unwrap_or("denied").to_string(),
                interrupt: false,
            }
        };

        let resp = req
            .build_response(behavior)
            .map_err(|e| Error::MessageParse {
                reason: format!("could not build control response: {e}"),
            })?;
        let mut line = serde_json::to_string(&resp).map_err(|e| Error::MessageParse {
            reason: format!("could not serialise control response: {e}"),
        })?;
        line.push('\n');
        self.sub.write_line(&line).await?;
        Ok(())
    }

    async fn write_unsupported_control_error(&mut self, req: &ControlRequest) -> Result<(), Error> {
        use crate::control::{ControlResponse, ControlResponseKind, ControlResponseType};
        let resp = ControlResponse {
            ty: ControlResponseType::ControlResponse,
            response: ControlResponseKind::Error {
                request_id: req.request_id.clone(),
                error: "unsupported control-request subtype".into(),
            },
        };
        let mut line = serde_json::to_string(&resp).map_err(|e| Error::MessageParse {
            reason: format!("error response serialise: {e}"),
        })?;
        line.push('\n');
        self.sub.write_line(&line).await
    }

    /// Graceful shutdown. Closes stdin, waits for the subprocess to exit.
    ///
    /// # Errors
    ///
    /// [`Error::Process`] when the subprocess exits non-zero, [`Error::Io`]
    /// for I/O failure.
    pub async fn disconnect(self) -> Result<(), Error> {
        self.sub.shutdown().await
    }
}
