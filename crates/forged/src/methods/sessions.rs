//! `sessions.*` method handlers — filesystem-level enumeration +
//! mutations over the on-disk session JSONL transcripts. Pure proxies
//! to the corresponding free functions in
//! [`forge_sdk::session::scan`] and
//! [`forge_sdk::session::mutations`].
//!
//! Each handler takes its `String` / `Option<String>` params by value
//! because the dispatch layer ([`crate::server`]) hands ownership of
//! deserialised JSON params straight through — pretending to borrow
//! them just hides the move and adds a `&` we'd immediately re-clone
//! when bubbling into the SDK. `clippy::needless_pass_by_value` is
//! noisy here; silence it module-wide.

#![allow(clippy::needless_pass_by_value)]

use forge_sdk::SDKSessionInfo;
use forge_sdk::SessionMessage;
use forge_sdk::session;

use crate::Error;

// =============================================================================
// Listing — M3.2 / M3.3 / M3.4
// =============================================================================

/// `sessions.list` — see wire spec §7.4.7.
///
/// `directory` is the project directory whose sessions to list. `None`
/// scans every project under the configured projects-dir.
///
/// # Errors
///
/// Currently infallible — [`forge_sdk::session::scan::list_sessions`]
/// returns the matching rows directly. Wrapped in `Result` so future
/// failure modes (e.g. permission errors when scanning a forbidden
/// directory) can be added without a breaking API change.
pub fn list(
    directory: Option<String>,
    limit: Option<usize>,
    offset: usize,
) -> Result<Vec<SDKSessionInfo>, Error> {
    Ok(session::scan::list_sessions(directory, limit, offset))
}

/// `sessions.info` — see wire spec §7.4.7.
///
/// # Errors
///
/// Infallible today; returns `Ok(None)` when the session id is unknown.
pub fn info(
    session_id: String,
    directory: Option<String>,
) -> Result<Option<SDKSessionInfo>, Error> {
    Ok(session::scan::get_session_info(&session_id, directory))
}

/// Return shape for `sessions.messages` — full transcript plus a
/// `watermark` cursor pointing at the highest-uuid message in the
/// transcript. Clients pass the watermark back as
/// [`session.subscribe`'s `since`](crate::methods::session::SubscribeParams::since)
/// argument when reconnecting (M3.3 lays the groundwork; the
/// daemon-side replay buffer lands in M5).
#[derive(Debug, Clone, serde::Serialize)]
#[non_exhaustive]
pub struct MessagesResult {
    /// Full transcript in chronological order.
    pub messages: Vec<SessionMessage>,
    /// Highest-uuid message in `messages`, or `None` for an empty
    /// transcript.
    pub watermark: Option<String>,
}

/// `sessions.messages` — see wire spec §7.4.7.
///
/// # Errors
///
/// Infallible today — returns an empty Vec + `None` watermark when the
/// transcript can't be located or is empty.
pub fn messages(session_id: String, directory: Option<String>) -> Result<MessagesResult, Error> {
    let messages = session::scan::get_session_messages(&session_id, directory);
    let watermark = messages.last().map(|m| m.uuid.clone());
    Ok(MessagesResult {
        messages,
        watermark,
    })
}

/// `sessions.list_subagents` — see wire spec §7.4.7.
///
/// # Errors
///
/// Infallible today.
pub fn list_subagents(session_id: String, directory: Option<String>) -> Result<Vec<String>, Error> {
    Ok(session::scan::list_subagents(&session_id, directory))
}

/// `sessions.subagent_messages` — see wire spec §7.4.7.
///
/// # Errors
///
/// Infallible today.
pub fn subagent_messages(
    session_id: String,
    subagent_id: String,
    directory: Option<String>,
) -> Result<Vec<SessionMessage>, Error> {
    Ok(session::scan::get_subagent_messages(
        &session_id,
        &subagent_id,
        directory,
        None,
        0,
    ))
}

/// `sessions.project_key` — see wire spec §7.4.7.
///
/// # Errors
///
/// Infallible today.
pub fn project_key(path: Option<String>) -> Result<String, Error> {
    Ok(session::scan::project_key_for_directory(path.as_deref()))
}

// =============================================================================
// Mutations — M3.5
// =============================================================================

/// `sessions.rename` — see wire spec §7.4.8.
///
/// # Errors
///
/// Bubbles [`forge_sdk::Error`] if the session jsonl is missing or
/// unwriteable.
pub fn rename(session_id: String, title: String, directory: Option<String>) -> Result<(), Error> {
    session::mutations::rename_session(&session_id, &title, directory.as_deref())
        .map_err(Error::Sdk)
}

/// `sessions.tag` — see wire spec §7.4.8. `tag = None` clears the tag.
///
/// # Errors
///
/// Bubbles [`forge_sdk::Error`].
pub fn tag(
    session_id: String,
    tag: Option<String>,
    directory: Option<String>,
) -> Result<(), Error> {
    session::mutations::tag_session(&session_id, tag.as_deref(), directory.as_deref())
        .map_err(Error::Sdk)
}

/// `sessions.delete` — see wire spec §7.4.8.
///
/// # Errors
///
/// Bubbles [`forge_sdk::Error`].
pub fn delete(session_id: String, directory: Option<String>) -> Result<(), Error> {
    session::mutations::delete_session(&session_id, directory.as_deref()).map_err(Error::Sdk)
}

/// Result of [`fork`].
#[derive(Debug, Clone, serde::Serialize)]
#[non_exhaustive]
pub struct ForkResult {
    /// UUID of the freshly-forked session.
    pub session_id: String,
}

/// `sessions.fork` — see wire spec §7.4.8.
///
/// `up_to_message_id` truncates the copy at the named message; `None`
/// copies the entire transcript. `title` overrides the auto-derived
/// fork title.
///
/// # Errors
///
/// Bubbles [`forge_sdk::Error`].
pub fn fork(
    session_id: String,
    up_to_message_id: Option<String>,
    title: Option<String>,
    directory: Option<String>,
) -> Result<ForkResult, Error> {
    let r = session::mutations::fork_session(
        &session_id,
        directory.as_deref(),
        up_to_message_id.as_deref(),
        title.as_deref(),
    )
    .map_err(Error::Sdk)?;
    Ok(ForkResult {
        session_id: r.session_id,
    })
}
