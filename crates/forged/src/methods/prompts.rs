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
/// its queue (consuming the queue's oneshot sender), and resolves the
/// outstanding-reverse entry (if any) directly via
/// [`crate::reverse_rpc::resolve`]. This is the atomic point of
/// commitment — the answer reaches the SDK-side handler synchronously,
/// without a spawned-task bridge that could race the timeout cleanup.
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

    // Direct resolve via the rev_id captured at park time. The
    // outstanding-reverse responder is the SDK-side handler's oneshot;
    // resolving it synchronously here closes the door on the timeout
    // path racing us. If the prompt was enqueued without a rev_id
    // (legacy path / direct test enqueue), fall back to sending
    // through the queue's responder so existing behavior is preserved.
    if let Some(rev_id) = prompt.rev_id.as_deref() {
        crate::reverse_rpc::resolve(state, rev_id, result);
    } else if prompt.responder.send(result).is_err() {
        // No rev_id and the queue receiver is gone — surface a debug
        // log so the timeout race is visible in operator traces.
        tracing::debug!(
            prompt_id = %prompt.prompt_id,
            "prompts.respond: responder receiver dropped (timeout race)"
        );
    }
    Ok(())
}
