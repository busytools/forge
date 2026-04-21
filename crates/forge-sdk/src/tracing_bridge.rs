//! Optional `tracing` bridge — helpers for creating turn-scoped spans.
//!
//! Mirrors Python SDK's v0.1.62+ tracing bridge. Use [`turn_span`] to get
//! a span to enter around each user turn; use [`tool_span`] around each
//! tool invocation.
//!
//! The bridge is designed for RAII — hold the returned `Entered` guard
//! for the duration of the work you want to scope.
//!
//! ```no_run
//! # fn example(session_id: &str) {
//! let span = forge_sdk::tracing_bridge::turn_span(session_id);
//! let _enter = span.enter();
//! // ... send user message, process events ...
//! # }
//! ```

use tracing::{Span, info_span};

/// Span covering one user turn.
#[must_use]
pub fn turn_span(session_id: &str) -> Span {
    info_span!("forge_sdk.turn", session_id = %session_id)
}

/// Span covering one tool invocation.
#[must_use]
pub fn tool_span(tool_name: &str, tool_use_id: &str) -> Span {
    info_span!(
        "forge_sdk.tool",
        tool_name = %tool_name,
        tool_use_id = %tool_use_id,
    )
}

/// Span covering one hook invocation.
#[must_use]
pub fn hook_span(kind: &str, callback_id: &str) -> Span {
    info_span!(
        "forge_sdk.hook",
        kind = %kind,
        callback_id = %callback_id,
    )
}
