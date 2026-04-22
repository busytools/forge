//! Pre-flight validation for [`Options::session_store`] combinations.
//!
//! Ports Python's `_internal/session_store_validation.py` (v0.1.64): a
//! pure function that [`Client::spawn`](crate::client::Client::spawn)
//! calls before any subprocess work so misconfigured combos fail fast
//! instead of surfacing as a confusing runtime error mid-session.

use crate::error::Error;
use crate::options::Options;

/// Reject invalid `session_store` option combinations before spawn.
///
/// Mirrors Python `validate_session_store_options`
/// (`_internal/session_store_validation.py:18-45`):
///
/// - `session_store` + `enable_file_checkpointing` — always rejected;
///   file checkpoints are local-disk-only and would diverge from the
///   mirrored transcript.
/// - `session_store` + `continue_conversation` without an override of
///   [`SessionStore::provides_list_sessions`](crate::session::store::SessionStore::provides_list_sessions)
///   — rejected unless `resume` is explicitly set (explicit resume
///   bypasses the code path that needs `list_sessions`).
/// - No `session_store` attached — always passes, regardless of other
///   option values.
///
/// # Errors
///
/// Returns [`Error::MessageParse`] with a reason string mirroring
/// Python's `ValueError` messages when a combination is invalid.
pub fn validate_session_store_options(options: &Options) -> Result<(), Error> {
    let Some(store) = &options.session_store else {
        return Ok(());
    };
    if options.enable_file_checkpointing {
        return Err(Error::message_parse(
            "session_store cannot be combined with enable_file_checkpointing \
             (checkpoints are local-disk only and would diverge from the \
             mirrored transcript)",
        ));
    }
    if options.continue_conversation && options.resume.is_none() && !store.provides_list_sessions()
    {
        return Err(Error::message_parse(
            "continue_conversation with session_store requires the store to \
             implement list_sessions()",
        ));
    }
    Ok(())
}
