//! WS+JSON-RPC client for forged.
//!
//! This is the TUI side of the wire — see
//! `~/.claude-stargate/plans/2026-04-25-forged-wire-spec.md`. It mirrors
//! the daemon's framing: outbound calls get a `req_<uuid>` id and await
//! a oneshot for the matching response; inbound notifications are
//! routed to per-session subscription channels; inbound reverse-RPC
//! requests (id starts `rev_`) are dispatched to user-supplied handlers
//! that are responsible for sending the response back via
//! [`Client::send_response`].

pub mod connection;

pub use connection::{Client, ClientError, Result};
