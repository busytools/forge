//! `prompts.*` method handlers.
//!
//! `prompts.respond` resolves a queued reverse-RPC by waking the
//! oneshot the SDK-side handler is awaiting on. The accompanying
//! `prompts.expired` notification is emitted from
//! [`crate::reverse_rpc::issue_to_primary`] on timeout — there is no
//! handler for it (notifications are server→client).

use crate::Error;
use crate::registry::DaemonState;
use crate::session_state::SessionId;

/// `prompts.respond` — resolve a queued reverse-RPC.
///
/// Looks up the named session, takes the prompt with `prompt_id` from
/// its queue (consuming the oneshot sender), and forwards `result`
/// over the channel. The awaiting reverse-RPC handler wakes and
/// returns the value to the SDK callback.
///
/// # Errors
///
/// - [`Error::SessionNotFound`] if `session_id` is unknown.
/// - [`Error::InvalidParams`] if `prompt_id` is not in the queue
///   (likely already expired or answered by a racing client).
pub fn respond(
    state: &DaemonState,
    session_id: &SessionId,
    prompt_id: &str,
    result: serde_json::Value,
) -> Result<(), Error> {
    let handle = state
        .get_session(session_id)
        .ok_or_else(|| Error::SessionNotFound(session_id.0.clone()))?;
    let Some(prompt) = handle.prompts.take(prompt_id) else {
        return Err(Error::InvalidParams(format!(
            "prompt_id {prompt_id} not in queue (expired or already answered)"
        )));
    };
    // Send through the oneshot — the awaiting reverse-RPC handler wakes.
    // If the receiver has been dropped (e.g. timeout fired between
    // `take` and `send`), the value is silently discarded — there's no
    // handler left to receive it. Surface a debug log so the timeout
    // race is visible in operator traces rather than vanishing silently.
    if prompt.responder.send(result).is_err() {
        tracing::debug!(
            prompt_id = %prompt.prompt_id,
            "prompts.respond: responder receiver dropped (timeout race)"
        );
    }
    Ok(())
}
