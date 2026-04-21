//! Public [`Client`] — the entry point consumers hold.

use tracing::debug;

use crate::Error;
use crate::messages::Message;
use crate::options::Options;
use crate::transport::codec::{decode_line, encode_user_prompt};
use crate::transport::process::Subprocess;

/// An active `claude` binary subprocess.
///
/// Construct via [`spawn`](Self::spawn). The first line the binary emits is
/// always a `system`/`init` message carrying the session id — `spawn`
/// consumes it so callers start clean at the first `assistant` turn.
#[derive(Debug)]
pub struct Client {
    sub: Subprocess,
    session_id: String,
    line_number: u64,
}

impl Client {
    /// Spawn `claude` with the given options and drain the init line.
    ///
    /// # Errors
    ///
    /// Any [`Error`] variant; see field docs.
    pub async fn spawn(options: Options) -> Result<Self, Error> {
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

    /// Read the next stream-json message from the subprocess.
    ///
    /// Returns `Ok(None)` at end-of-stream (subprocess exited).
    ///
    /// # Errors
    ///
    /// - [`Error::JsonDecode`] / [`Error::MessageParse`] per line.
    /// - [`Error::Io`] on pipe read failure.
    pub async fn next_event(&mut self) -> Result<Option<Message>, Error> {
        let Some(line) = self.sub.read_line().await? else {
            return Ok(None);
        };
        self.line_number += 1;
        let msg = decode_line(&line, self.line_number)?;
        Ok(Some(msg))
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
