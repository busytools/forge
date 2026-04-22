//! Drive the shipped conformance harness
//! ([`forge_sdk::testing::run_session_store_conformance`]) against
//! forge-sdk's own `MemorySessionStore`. Mirrors Python upstream's
//! `TestInMemorySessionStore::test_conformance`.
//!
//! This closes the 5 previously-ignored `session_store_conformance`
//! parity tests: once the harness exists, the in-memory store
//! passing is the direct analogue of upstream's test.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use forge_sdk::testing::run_session_store_conformance;
use forge_sdk::{
    InMemorySessionStore, SessionKey, SessionListSubkeysKey, SessionStore, SessionStoreEntry,
    SessionStoreError,
};

/// `test_conformance` — drives the full harness against the shipped
/// `MemorySessionStore`. This is the most important parity check:
/// our own store must pass the public conformance contract.
#[tokio::test]
async fn inmemory_store_conformance() {
    run_session_store_conformance(
        || Arc::new(InMemorySessionStore::default()) as Arc<dyn SessionStore>,
        &HashSet::new(),
    )
    .await;
}

/// `test_skip_optional_suppresses_contracts` — a minimal store
/// implementing only `append` + `load` passes when optionals are
/// skipped.
#[tokio::test]
async fn minimal_store_with_skip_optional() {
    #[derive(Default)]
    struct MinimalStore {
        data: tokio::sync::Mutex<std::collections::HashMap<String, Vec<SessionStoreEntry>>>,
    }

    impl MinimalStore {
        fn key_str(key: &SessionKey) -> String {
            let sub = key.subpath.as_deref().unwrap_or("");
            format!("{}/{}/{sub}", key.project_key, key.session_id)
        }
    }

    #[async_trait]
    impl SessionStore for MinimalStore {
        async fn append(
            &self,
            key: &SessionKey,
            entries: &[SessionStoreEntry],
        ) -> Result<(), SessionStoreError> {
            let k = Self::key_str(key);
            let mut data = self.data.lock().await;
            data.entry(k).or_default().extend_from_slice(entries);
            Ok(())
        }

        async fn load(
            &self,
            key: &SessionKey,
        ) -> Result<Option<Vec<SessionStoreEntry>>, SessionStoreError> {
            let k = Self::key_str(key);
            let data = self.data.lock().await;
            Ok(data.get(&k).cloned())
        }
    }

    let skip: HashSet<&str> = [
        "list_sessions",
        "list_session_summaries",
        "delete",
        "list_subkeys",
    ]
    .into_iter()
    .collect();
    run_session_store_conformance(
        || Arc::new(MinimalStore::default()) as Arc<dyn SessionStore>,
        &skip,
    )
    .await;
}

/// `test_auto_skips_unimplemented_optionals` — a minimal store that
/// doesn't override the optionals gets them auto-skipped by the
/// harness, without the caller naming them in `skip_optional`.
#[tokio::test]
async fn minimal_store_auto_skips_unimplemented() {
    #[derive(Default)]
    struct AutoSkipStore {
        data: tokio::sync::Mutex<std::collections::HashMap<String, Vec<SessionStoreEntry>>>,
    }

    impl AutoSkipStore {
        fn key_str(key: &SessionKey) -> String {
            let sub = key.subpath.as_deref().unwrap_or("");
            format!("{}/{}/{sub}", key.project_key, key.session_id)
        }
    }

    #[async_trait]
    impl SessionStore for AutoSkipStore {
        async fn append(
            &self,
            key: &SessionKey,
            entries: &[SessionStoreEntry],
        ) -> Result<(), SessionStoreError> {
            let k = Self::key_str(key);
            let mut data = self.data.lock().await;
            data.entry(k).or_default().extend_from_slice(entries);
            Ok(())
        }

        async fn load(
            &self,
            key: &SessionKey,
        ) -> Result<Option<Vec<SessionStoreEntry>>, SessionStoreError> {
            let k = Self::key_str(key);
            let data = self.data.lock().await;
            Ok(data.get(&k).cloned())
        }

        // Deliberately NOT overriding list_sessions / delete /
        // list_subkeys / list_session_summaries — defaults return
        // NotImplemented so the harness skips the related contracts.
    }

    run_session_store_conformance(
        || Arc::new(AutoSkipStore::default()) as Arc<dyn SessionStore>,
        &HashSet::new(),
    )
    .await;
}

/// `test_has_optional_rejects_unknown_method` — the harness
/// asserts that `skip_optional` entries are members of the known
/// optional set; an unknown entry panics.
#[tokio::test]
#[should_panic(expected = "unknown optional methods")]
async fn rejects_unknown_optional_method() {
    let skip: HashSet<&str> = ["bogus_method"].into_iter().collect();
    run_session_store_conformance(
        || Arc::new(InMemorySessionStore::default()) as Arc<dyn SessionStore>,
        &skip,
    )
    .await;
}

// Helper: avoid compiler dead-code warning on SessionListSubkeysKey
// since the tests above don't construct one directly (the harness
// does internally).
#[allow(dead_code)]
fn _touch_subkeys_key() -> SessionListSubkeysKey {
    SessionListSubkeysKey {
        project_key: String::new(),
        session_id: String::new(),
    }
}
