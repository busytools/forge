//! Direct `forge_sdk::Message` consumer for the App.
//!
//! Phase 1.2 of the bridge-collapse refactor. Today the
//! `agent::bridge::message_handlers` module owns SDK-message
//! unpacking — it walks `Message::Assistant.content`, pairs
//! `tool_use` ↔ `tool_result` across messages, and emits
//! `SessionUpdate` events the App consumes through `events::client`.
//!
//! This module introduces the App-side replacement: a top-level
//! [`handle_sdk_message`] dispatcher that the bridge worker (after
//! Phase 1.3) feeds raw `forge_sdk::Message` envelopes to. Per-variant
//! handlers below are no-op stubs in Phase 1; Phase 2 progressively
//! moves the unpacking + state-mutation logic out of the bridge into
//! these stubs, one variant per commit, until the bridge module is
//! dead code (Phase 3).
//!
//! See `~/.claude-nf/plans/pick-up-where-we-quirky-grove.md` for the
//! per-variant cutover order.
//!
//! # Temporary clippy allows
//!
//! - `needless_pass_by_value`: every per-variant handler takes
//!   `msg: Message` by value but doesn't consume it during Phase 1.
//!   Phase 2 destructures it for state mutation; the warning
//!   resolves naturally as each handler is filled in.
//! - `missing_panics_doc` / `missing_errors_doc`: handlers don't
//!   panic and aren't `Result`-returning, but doc-only lints inside
//!   `forge-tui`'s pedantic config flag the doc comments.
#![allow(clippy::needless_pass_by_value, clippy::doc_markdown)]
//!
//! # Why a parallel path during Phase 1?
//!
//! Phase 1 is compile-safe and behaviour-neutral: the bridge keeps
//! emitting `SessionUpdate`s as before, App keeps consuming them,
//! and `BridgeEvent::SdkMessage` events flow alongside as a no-op
//! double-feed. Phase 2 cutovers each variant atomically: the
//! bridge stops emitting the SessionUpdate variant, this module's
//! handler starts mutating App state. No double-write window per
//! variant.

use forge_sdk::Message;

use crate::app::App;

/// Top-level entry point. Called from `events::client` after the
/// session-id check on `ClientEvent::SdkMessageReceived`. Dispatches
/// to per-variant handlers below.
///
/// During Phase 1 every handler is a no-op — the bridge module's
/// existing `handle_sdk_message` (in `agent::bridge::message_handlers`)
/// continues to do the real work. Phase 2 fills these in per variant.
pub(super) fn handle_sdk_message(app: &mut App, msg: Message) {
    match msg {
        Message::Assistant { .. } => handle_assistant(app, msg),
        Message::User { .. } => handle_user(app, msg),
        Message::System { .. } => handle_system(app, msg),
        Message::TaskStarted { .. } => handle_task_started(app, msg),
        Message::TaskProgress { .. } => handle_task_progress(app, msg),
        Message::TaskNotification { .. } => handle_task_notification(app, msg),
        Message::RateLimitEvent { .. } => handle_rate_limit_event(app, msg),
        Message::Result { .. } => handle_result(app, msg),
        Message::StreamEvent { .. } => handle_stream_event(app, msg),
        // forge_sdk::Message is `#[non_exhaustive]` — Error / Unknown
        // and any future variants fall through here.
        _ => handle_unknown(app, msg),
    }
}

// Per-variant handlers — Phase 1 stubs. Each takes ownership of the
// full `Message` so Phase 2 can destructure freely without revisiting
// the dispatcher. The `_ = app; _ = msg;` lines suppress unused-arg
// warnings until Phase 2.

fn handle_assistant(app: &mut App, msg: Message) {
    let _ = app;
    let _ = msg;
}

fn handle_user(app: &mut App, msg: Message) {
    let _ = app;
    let _ = msg;
}

fn handle_system(app: &mut App, msg: Message) {
    let _ = app;
    let _ = msg;
}

fn handle_task_started(app: &mut App, msg: Message) {
    let _ = app;
    let _ = msg;
}

fn handle_task_progress(app: &mut App, msg: Message) {
    let _ = app;
    let _ = msg;
}

fn handle_task_notification(app: &mut App, msg: Message) {
    let _ = app;
    let _ = msg;
}

fn handle_rate_limit_event(app: &mut App, msg: Message) {
    let _ = app;
    let _ = msg;
}

fn handle_result(app: &mut App, msg: Message) {
    let _ = app;
    let _ = msg;
}

fn handle_stream_event(app: &mut App, msg: Message) {
    let _ = app;
    let _ = msg;
}

fn handle_unknown(app: &mut App, msg: Message) {
    let _ = app;
    let _ = msg;
}
