//! Python-SDK parity test suite.
//!
//! Each submodule mirrors one file from
//! <https://github.com/anthropics/claude-agent-sdk-python>'s `tests/`
//! directory. Every test is tagged with the upstream file and test name
//! it ports from — so a weekly `grep` against the current upstream
//! version answers "have we mirrored this?" without guesswork. See
//! `PARITY.md`'s "Test-mirroring strategy" for the full contract.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "python_parity/client.rs"]
mod client;

#[path = "python_parity/errors.rs"]
mod errors;

#[path = "python_parity/integration.rs"]
mod integration;

#[path = "python_parity/mcp_large_output.rs"]
mod mcp_large_output;

#[path = "python_parity/message_parser.rs"]
mod message_parser;

#[path = "python_parity/session_store_conformance.rs"]
mod session_store_conformance;

#[path = "python_parity/transport.rs"]
mod transport;

#[path = "python_parity/rate_limit.rs"]
mod rate_limit;

#[path = "python_parity/subprocess_buffering.rs"]
mod subprocess_buffering;

#[path = "python_parity/types.rs"]
mod types;
