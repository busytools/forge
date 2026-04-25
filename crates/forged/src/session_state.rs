//! Per-session state held inside the daemon.
//!
//! Each registered session owns a single actor task that exclusively holds
//! the [`forge_sdk::Client`]. Dispatch handlers communicate with the actor
//! over an mpsc command channel rather than locking the
//! [`forge_sdk::Client`] directly — locking across `next_event` would
//! deadlock writers, since [`forge_sdk::Client::next_event`] blocks on
//! subprocess I/O while still holding `&mut self`.

use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::Error;
use crate::connection::ConnectionId;

/// Session id minted by forged on `session.spawn`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

/// Commands the dispatch handlers send to a session's actor task.
///
/// Each command carries a [`oneshot::Sender`] for the actor's reply so the
/// dispatcher can `.await` the result and surface the right JSON-RPC
/// error code. Variants are added as new wire methods land in subsequent
/// milestones.
#[derive(Debug)]
#[non_exhaustive]
pub enum Command {
    /// Forward a user prompt to the underlying claude.
    SendUserMessage {
        /// Prompt body.
        prompt: String,
        /// Reply channel for the actor's send result.
        reply: oneshot::Sender<Result<(), Error>>,
    },
    /// Close the subprocess's stdin so it can flush its final result frame
    /// and exit.
    EndInput {
        /// Reply channel for the actor's `end_input` result.
        reply: oneshot::Sender<Result<(), Error>>,
    },
    /// Tear down the underlying [`forge_sdk::Client`] gracefully and exit
    /// the actor loop.
    Disconnect {
        /// Reply channel for the actor's `disconnect` result.
        reply: oneshot::Sender<Result<(), Error>>,
    },
}

/// Per-session state visible to dispatch handlers + the broadcast helper.
///
/// The actual [`forge_sdk::Client`] lives inside the actor task spawned at
/// registration time; dispatch handlers send [`Command`]s through
/// [`SessionState::commands`] to drive it.
#[derive(Debug)]
#[non_exhaustive]
pub struct SessionState {
    /// Daemon-minted session id (`sess_<uuid>`).
    pub id: SessionId,
    /// Command channel into the session's actor task.
    pub commands: mpsc::UnboundedSender<Command>,
    /// Connection IDs subscribed to this session's events. Broadcast target.
    pub subscribers: Mutex<Vec<ConnectionId>>,
    /// Primary client (single-client model in M2; multi-client lands in M5).
    pub primary: Mutex<Option<ConnectionId>>,
}

impl SessionState {
    /// Construct fresh session state given its actor's command sender.
    #[must_use]
    pub fn new(id: SessionId, commands: mpsc::UnboundedSender<Command>) -> Self {
        Self {
            id,
            commands,
            subscribers: Mutex::new(Vec::new()),
            primary: Mutex::new(None),
        }
    }
}

/// Reference-counted handle to a [`SessionState`].
pub type SessionHandle = Arc<SessionState>;
