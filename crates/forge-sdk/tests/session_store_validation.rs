//! Pre-flight validation for `ClaudeAgentOptions.session_store` combos.
//! Ports `tests/test_session_store_conformance.py` fail-fast semantics
//! from `claude-agent-sdk-python` v0.1.64
//! (`_internal/session_store_validation.py:40-45`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use forge_sdk::{Client, Error, MemorySessionStore, OptionsBuilder};

/// Rule 1 (Python `session_store_validation.py:28-38`): a store
/// without `list_sessions` cannot drive `continue_conversation` unless
/// `resume` is explicitly set.
#[tokio::test]
async fn continue_conversation_without_list_sessions_is_rejected() {
    use async_trait::async_trait;
    use forge_sdk::{SessionKey, SessionListSubkeysKey, SessionStore, SessionStoreEntry};
    use std::sync::Arc;

    #[derive(Debug)]
    struct MinimalStore;

    #[async_trait]
    impl SessionStore for MinimalStore {
        async fn append(
            &self,
            _key: &SessionKey,
            _entries: &[SessionStoreEntry],
        ) -> Result<(), forge_sdk::SessionStoreError> {
            Ok(())
        }
        async fn load(
            &self,
            _key: &SessionKey,
        ) -> Result<Option<Vec<SessionStoreEntry>>, forge_sdk::SessionStoreError> {
            Ok(None)
        }
        async fn list_subkeys(
            &self,
            _key: &SessionListSubkeysKey,
        ) -> Result<Vec<String>, forge_sdk::SessionStoreError> {
            Ok(Vec::new())
        }
        // Deliberately NOT overriding `provides_list_sessions` or
        // `list_sessions` — simulates a bare-minimum impl.
    }

    let opts = OptionsBuilder::new()
        .binary("/nonexistent/claude")
        .session_store_arc(Arc::new(MinimalStore))
        .continue_conversation(true)
        .build();
    let err = Client::spawn(opts).await.expect_err("must reject");
    match err {
        Error::MessageParse { reason, .. } => {
            assert!(
                reason.contains("list_sessions"),
                "reason must mention list_sessions, got: {reason}"
            );
        }
        other => panic!("expected MessageParse, got {other:?}"),
    }
}

/// Conversely, a `MinimalStore` WITH an explicit `resume` target is
/// accepted — `list_sessions` is provably unreachable in that path.
#[tokio::test]
async fn continue_conversation_with_resume_bypasses_list_sessions_check() {
    use async_trait::async_trait;
    use forge_sdk::{SessionKey, SessionListSubkeysKey, SessionStore, SessionStoreEntry};
    use std::sync::Arc;

    #[derive(Debug)]
    struct MinimalStore;

    #[async_trait]
    impl SessionStore for MinimalStore {
        async fn append(
            &self,
            _key: &SessionKey,
            _entries: &[SessionStoreEntry],
        ) -> Result<(), forge_sdk::SessionStoreError> {
            Ok(())
        }
        async fn load(
            &self,
            _key: &SessionKey,
        ) -> Result<Option<Vec<SessionStoreEntry>>, forge_sdk::SessionStoreError> {
            Ok(None)
        }
        async fn list_subkeys(
            &self,
            _key: &SessionListSubkeysKey,
        ) -> Result<Vec<String>, forge_sdk::SessionStoreError> {
            Ok(Vec::new())
        }
    }

    let opts = OptionsBuilder::new()
        .binary("/nonexistent/claude")
        .session_store_arc(Arc::new(MinimalStore))
        .continue_conversation(true)
        .resume("sess-explicit-resume")
        .build();
    // Spawn will fail on the binary path, NOT on the validation rule.
    // We assert the error is NOT the validation message.
    let err = Client::spawn(opts).await.expect_err("binary missing");
    if let Error::MessageParse { reason, .. } = &err {
        assert!(
            !reason.contains("list_sessions"),
            "explicit resume must bypass the list_sessions check"
        );
    }
}

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
