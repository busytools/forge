//! Pre-flight validation for `ClaudeAgentOptions.session_store` combos.
//! Ports `tests/test_session_store_conformance.py` fail-fast semantics
//! from `claude-agent-sdk-python` v0.1.64
//! (`_internal/session_store_validation.py:40-45`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use forge_sdk::{Client, Error, MemorySessionStore, OptionsBuilder};

#[tokio::test]
async fn session_store_plus_file_checkpointing_is_rejected() {
    let opts = OptionsBuilder::new()
        .binary("/nonexistent/claude")
        .session_store_arc(Arc::new(MemorySessionStore::default()))
        .enable_file_checkpointing(true)
        .build();
    let err = Client::spawn(opts).await.expect_err("must reject");
    match err {
        Error::MessageParse { reason, .. } => {
            assert!(
                reason.contains("enable_file_checkpointing"),
                "reason must mention enable_file_checkpointing, got: {reason}"
            );
            assert!(
                reason.contains("session_store"),
                "reason must mention session_store, got: {reason}"
            );
        }
        other => panic!("expected MessageParse, got {other:?}"),
    }
}
