#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::implicit_hasher,
    clippy::too_many_lines
)]

//! Shared conformance suite for [`SessionStore`] adapters.
//!
//! Ports Python SDK v0.1.64
//! `claude_agent_sdk.testing.session_store_conformance`. Third-party
//! store authors call [`run_session_store_conformance`] from an async
//! test to assert the 14 behavioural contracts every adapter must
//! satisfy.
//!
//! Contracts for optional methods (`list_sessions`,
//! `list_session_summaries`, `delete`, `list_subkeys`) are skipped
//! when explicitly named in `skip_optional` OR when the store's
//! default trait impl raises [`SessionStoreError::NotImplemented`]
//! (we auto-probe with a safe no-side-effect call).
//!
//! # Example
//!
//! ```no_run
//! # use forge_sdk::{SessionStore, InMemorySessionStore};
//! # use forge_sdk::testing::run_session_store_conformance;
//! # use std::collections::HashSet;
//! #[tokio::test]
//! async fn my_store_conformance() {
//!     run_session_store_conformance(
//!         || std::sync::Arc::new(InMemorySessionStore::default()) as std::sync::Arc<dyn SessionStore>,
//!         &HashSet::new(),
//!     )
//!     .await;
//! }
//! ```

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use serde_json::{Value, json};

use crate::session::store::{
    SessionKey, SessionListSubkeysKey, SessionStore, SessionStoreEntry, SessionStoreError,
};
use crate::session::summary::fold_session_summary;

/// The four optional `SessionStore` methods the conformance harness
/// can skip. Pass any of these to `skip_optional` as `&str`.
pub const OPTIONAL_METHODS: [&str; 4] = [
    "list_sessions",
    "list_session_summaries",
    "delete",
    "list_subkeys",
];

/// Factory-producing convenience alias. Every contract gets a fresh
/// store from `factory()` to keep contracts isolated.
type StoreFactory = dyn Fn() -> Arc<dyn SessionStore> + Send + Sync;

/// Run the full 14-contract `SessionStore` conformance suite against
/// `factory`. Panics (via `assert!`) on the first failure.
///
/// `factory` is invoked once per contract to provide isolation.
/// Optional contracts are auto-probed: if the store returns
/// [`SessionStoreError::NotImplemented`] for a probe call, that
/// contract's assertions are skipped.
///
/// # Panics
///
/// Any contract violation panics with a descriptive message. This is
/// the intended surface for a test harness — failures land as test
/// failures rather than `Result` returns.
pub async fn run_session_store_conformance<F>(factory: F, skip_optional: &HashSet<&str>)
where
    F: Fn() -> Arc<dyn SessionStore> + Send + Sync + 'static,
{
    let invalid: Vec<&&str> = skip_optional
        .iter()
        .filter(|name| !OPTIONAL_METHODS.contains(name))
        .collect();
    assert!(
        invalid.is_empty(),
        "unknown optional methods in skip_optional: {invalid:?}"
    );

    let factory: Arc<StoreFactory> = Arc::new(factory);
    let fresh = || factory.clone()();

    let probe = fresh();
    let has_list_sessions = has_optional(probe.as_ref(), "list_sessions", skip_optional).await;
    let has_list_summaries =
        has_optional(probe.as_ref(), "list_session_summaries", skip_optional).await;
    let has_delete = has_optional(probe.as_ref(), "delete", skip_optional).await;
    let has_list_subkeys = has_optional(probe.as_ref(), "list_subkeys", skip_optional).await;

    required_append_load(&fresh).await;

    if has_list_sessions {
        optional_list_sessions(&fresh).await;
    }
    if has_list_summaries {
        optional_list_session_summaries(&fresh, has_list_sessions, has_delete).await;
    }
    if has_delete {
        optional_delete(&fresh, has_list_subkeys, has_list_sessions).await;
    }
    if has_list_subkeys {
        optional_list_subkeys(&fresh).await;
    }
}

// ---------------------------------------------------------------------
// Probe helpers
// ---------------------------------------------------------------------

async fn has_optional(
    store: &dyn SessionStore,
    method: &str,
    skip_optional: &HashSet<&str>,
) -> bool {
    if skip_optional.contains(method) {
        return false;
    }
    match method {
        "list_sessions" => store.provides_list_sessions(),
        "list_session_summaries" => !matches!(
            store.list_session_summaries("__probe__").await,
            Err(SessionStoreError::NotImplemented)
        ),
        "delete" => {
            // Probe by deleting a non-existent key on a throw-away key
            // namespace. Implementations that support delete will
            // return Ok for unknown keys; NotImplemented signals the
            // default stub.
            let probe_key = SessionKey {
                project_key: "__probe__".into(),
                session_id: "__probe__".into(),
                subpath: None,
            };
            !matches!(
                store.delete(&probe_key).await,
                Err(SessionStoreError::NotImplemented)
            )
        }
        "list_subkeys" => {
            let probe = SessionListSubkeysKey {
                project_key: "__probe__".into(),
                session_id: "__probe__".into(),
            };
            !matches!(
                store.list_subkeys(&probe).await,
                Err(SessionStoreError::NotImplemented)
            )
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------
// Required contracts: append + load (6 assertions)
// ---------------------------------------------------------------------

async fn required_append_load<F>(fresh: &F)
where
    F: Fn() -> Arc<dyn SessionStore>,
{
    // 1. append then load returns same entries in same order.
    let store = fresh();
    let key = default_key();
    store
        .append(
            &key,
            &[
                entry(json!({"uuid": "b", "n": 1})),
                entry(json!({"uuid": "a", "n": 2})),
            ],
        )
        .await
        .expect("append");
    let loaded = store.load(&key).await.expect("load").expect("some");
    assert_eq!(loaded.len(), 2, "contract 1: append+load size");
    assert_entry_eq(&loaded[0], &json!({"uuid": "b", "n": 1}));
    assert_entry_eq(&loaded[1], &json!({"uuid": "a", "n": 2}));

    // 2. load unknown key returns None.
    let store = fresh();
    let unknown = SessionKey {
        project_key: "proj".into(),
        session_id: "nope".into(),
        subpath: None,
    };
    assert!(
        store.load(&unknown).await.expect("load").is_none(),
        "contract 2: load unknown returns None"
    );
    store
        .append(&key, &[entry(json!({"uuid": "x", "n": 1}))])
        .await
        .expect("append");
    let unknown_sub = SessionKey {
        project_key: key.project_key.clone(),
        session_id: key.session_id.clone(),
        subpath: Some("nope".into()),
    };
    assert!(
        store.load(&unknown_sub).await.expect("load").is_none(),
        "contract 2b: load unknown subpath returns None"
    );

    // 3. multiple append calls preserve call order.
    let store = fresh();
    store
        .append(&key, &[entry(json!({"uuid": "z", "n": 1}))])
        .await
        .expect("append1");
    store
        .append(
            &key,
            &[
                entry(json!({"uuid": "a", "n": 2})),
                entry(json!({"uuid": "m", "n": 3})),
            ],
        )
        .await
        .expect("append2");
    store
        .append(&key, &[entry(json!({"uuid": "b", "n": 4}))])
        .await
        .expect("append3");
    let loaded = store.load(&key).await.expect("load").expect("some");
    assert_eq!(loaded.len(), 4, "contract 3: total entry count");
    assert_entry_eq(&loaded[0], &json!({"uuid": "z", "n": 1}));
    assert_entry_eq(&loaded[1], &json!({"uuid": "a", "n": 2}));
    assert_entry_eq(&loaded[2], &json!({"uuid": "m", "n": 3}));
    assert_entry_eq(&loaded[3], &json!({"uuid": "b", "n": 4}));

    // 4. append([]) is a no-op.
    let store = fresh();
    store
        .append(&key, &[entry(json!({"uuid": "a", "n": 1}))])
        .await
        .expect("append");
    store.append(&key, &[]).await.expect("append empty");
    let loaded = store.load(&key).await.expect("load").expect("some");
    assert_eq!(loaded.len(), 1, "contract 4: append([]) is no-op");

    // 5. subpath keys are stored independently of main.
    let store = fresh();
    let sub = SessionKey {
        project_key: key.project_key.clone(),
        session_id: key.session_id.clone(),
        subpath: Some("subagents/agent-1".into()),
    };
    store
        .append(&key, &[entry(json!({"uuid": "m", "n": 1}))])
        .await
        .expect("append main");
    store
        .append(&sub, &[entry(json!({"uuid": "s", "n": 1}))])
        .await
        .expect("append sub");
    let main = store.load(&key).await.expect("load").expect("some");
    let sub_loaded = store.load(&sub).await.expect("load").expect("some");
    assert_eq!(main.len(), 1, "contract 5: main isolation");
    assert_eq!(sub_loaded.len(), 1, "contract 5: sub isolation");

    // 6. project_key isolation.
    let store = fresh();
    let key_a = SessionKey {
        project_key: "A".into(),
        session_id: "s1".into(),
        subpath: None,
    };
    let key_b = SessionKey {
        project_key: "B".into(),
        session_id: "s1".into(),
        subpath: None,
    };
    store
        .append(&key_a, &[entry(json!({"from": "A"}))])
        .await
        .expect("append A");
    store
        .append(&key_b, &[entry(json!({"from": "B"}))])
        .await
        .expect("append B");
    let loaded_a = store.load(&key_a).await.expect("load").expect("some");
    let loaded_b = store.load(&key_b).await.expect("load").expect("some");
    assert_entry_eq(&loaded_a[0], &json!({"from": "A"}));
    assert_entry_eq(&loaded_b[0], &json!({"from": "B"}));
}

// ---------------------------------------------------------------------
// Optional: list_sessions (2 contracts)
// ---------------------------------------------------------------------

async fn optional_list_sessions<F>(fresh: &F)
where
    F: Fn() -> Arc<dyn SessionStore>,
{
    // 7. list_sessions returns session_ids for project.
    let store = fresh();
    store
        .append(
            &SessionKey {
                project_key: "proj".into(),
                session_id: "a".into(),
                subpath: None,
            },
            &[entry(json!({"n": 1}))],
        )
        .await
        .expect("append a");
    store
        .append(
            &SessionKey {
                project_key: "proj".into(),
                session_id: "b".into(),
                subpath: None,
            },
            &[entry(json!({"n": 1}))],
        )
        .await
        .expect("append b");
    store
        .append(
            &SessionKey {
                project_key: "other".into(),
                session_id: "c".into(),
                subpath: None,
            },
            &[entry(json!({"n": 1}))],
        )
        .await
        .expect("append other");
    let sessions = store.list_sessions("proj").await.expect("list");
    let mut ids: Vec<_> = sessions.iter().map(|e| e.session_id.clone()).collect();
    ids.sort();
    assert_eq!(
        ids,
        vec!["a".to_string(), "b".to_string()],
        "contract 7: list scope"
    );
    assert!(
        sessions.iter().all(|e| e.mtime > 1_000_000_000_000),
        "contract 7: mtime must be epoch-ms (>1e12)"
    );
    assert!(
        store
            .list_sessions("never-appended-project")
            .await
            .expect("list empty")
            .is_empty(),
        "contract 7: empty project list"
    );

    // 8. list_sessions excludes subagent subpaths.
    let store = fresh();
    let key = SessionKey {
        project_key: "proj".into(),
        session_id: "main".into(),
        subpath: None,
    };
    let sub = SessionKey {
        project_key: "proj".into(),
        session_id: "main".into(),
        subpath: Some("subagents/agent-1".into()),
    };
    store
        .append(&key, &[entry(json!({"n": 1}))])
        .await
        .expect("append main");
    store
        .append(&sub, &[entry(json!({"n": 1}))])
        .await
        .expect("append sub");
    let sessions = store.list_sessions("proj").await.expect("list");
    let ids: Vec<_> = sessions.iter().map(|e| e.session_id.clone()).collect();
    assert_eq!(ids, vec!["main".to_string()], "contract 8: main-only");
}

// ---------------------------------------------------------------------
// Optional: list_session_summaries (contract 14)
// ---------------------------------------------------------------------

async fn optional_list_session_summaries<F>(fresh: &F, has_list_sessions: bool, has_delete: bool)
where
    F: Fn() -> Arc<dyn SessionStore>,
{
    let store = fresh();
    let key = SessionKey {
        project_key: "proj".into(),
        session_id: "summ-sess".into(),
        subpath: None,
    };
    store
        .append(
            &key,
            &[
                entry(json!({"timestamp": "2024-01-01T00:00:00.000Z", "customTitle": "first"})),
                entry(json!({"timestamp": "2024-01-01T00:00:01.000Z"})),
            ],
        )
        .await
        .expect("append 1");
    store
        .append(
            &key,
            &[entry(
                json!({"timestamp": "2024-01-01T00:00:02.000Z", "customTitle": "second"}),
            )],
        )
        .await
        .expect("append 2");
    store
        .append(
            &SessionKey {
                project_key: "other".into(),
                session_id: "elsewhere".into(),
                subpath: None,
            },
            &[entry(json!({"timestamp": "2024-01-01T00:00:00.000Z"}))],
        )
        .await
        .expect("append elsewhere");
    let summaries = store
        .list_session_summaries("proj")
        .await
        .expect("list_session_summaries");
    let by_id: BTreeMap<_, _> = summaries
        .iter()
        .map(|s| (s.session_id.clone(), s))
        .collect();
    assert_eq!(
        by_id.keys().cloned().collect::<Vec<_>>(),
        vec!["summ-sess".to_string()],
        "contract 14: summaries scoped to project"
    );
    let summ = by_id["summ-sess"];
    assert!(
        summ.mtime > 1_000_000_000_000,
        "contract 14: sidecar mtime must be epoch-ms"
    );

    if has_list_sessions {
        let ls_by_id: BTreeMap<_, _> = store
            .list_sessions("proj")
            .await
            .expect("list")
            .into_iter()
            .map(|e| (e.session_id, e.mtime))
            .collect();
        assert!(
            summ.mtime >= ls_by_id["summ-sess"],
            "contract 14: sidecar mtime must share list_sessions clock (sidecar >= list)"
        );
    }

    assert!(
        !summ.data.is_empty(),
        "contract 14: summary data must be non-empty after appends"
    );
    let refolded = fold_session_summary(
        Some(summ),
        &key,
        &[entry(json!({"timestamp": "2024-01-01T00:00:03.000Z"}))],
    );
    assert_eq!(refolded.session_id, "summ-sess");
    assert_eq!(
        refolded.mtime, summ.mtime,
        "contract 14: fold preserves prev.mtime verbatim"
    );

    // Subagent appends must NOT affect main session's summary.
    let sub = SessionKey {
        project_key: key.project_key.clone(),
        session_id: key.session_id.clone(),
        subpath: Some("subagents/agent-1".into()),
    };
    store
        .append(
            &sub,
            &[entry(
                json!({"timestamp": "2024-01-01T00:00:09.000Z", "customTitle": "subagent"}),
            )],
        )
        .await
        .expect("append subagent");
    let after_sub = store
        .list_session_summaries("proj")
        .await
        .expect("list post-sub");
    let after = after_sub
        .iter()
        .find(|s| s.session_id == "summ-sess")
        .expect("main summary still present");
    assert_eq!(
        after.data, summ.data,
        "contract 14: subagent append must not mutate main summary data"
    );

    assert!(
        store
            .list_session_summaries("never-appended-project")
            .await
            .expect("list empty")
            .is_empty(),
        "contract 14: empty-project summaries"
    );
    if has_delete {
        store.delete(&key).await.expect("delete");
        assert!(
            store
                .list_session_summaries("proj")
                .await
                .expect("list post-delete")
                .is_empty(),
            "contract 14: summaries cleared after delete"
        );
    }
}

// ---------------------------------------------------------------------
// Optional: delete (3 contracts)
// ---------------------------------------------------------------------

async fn optional_delete<F>(fresh: &F, has_list_subkeys: bool, has_list_sessions: bool)
where
    F: Fn() -> Arc<dyn SessionStore>,
{
    // 9. delete main then load returns None.
    let store = fresh();
    let never = SessionKey {
        project_key: "proj".into(),
        session_id: "never-written".into(),
        subpath: None,
    };
    store.delete(&never).await.expect("delete never");
    let key = default_key();
    store
        .append(&key, &[entry(json!({"n": 1}))])
        .await
        .expect("append");
    store.delete(&key).await.expect("delete");
    assert!(
        store.load(&key).await.expect("load").is_none(),
        "contract 9: delete then load is None"
    );

    // 10. delete main cascades to subkeys.
    let store = fresh();
    let sub1 = SessionKey {
        project_key: key.project_key.clone(),
        session_id: key.session_id.clone(),
        subpath: Some("subagents/agent-1".into()),
    };
    let sub2 = SessionKey {
        project_key: key.project_key.clone(),
        session_id: key.session_id.clone(),
        subpath: Some("subagents/agent-2".into()),
    };
    let other = SessionKey {
        project_key: "proj".into(),
        session_id: "sess2".into(),
        subpath: None,
    };
    let other_proj = SessionKey {
        project_key: "other-proj".into(),
        session_id: key.session_id.clone(),
        subpath: None,
    };
    store.append(&key, &[entry(json!({"n": 1}))]).await.unwrap();
    store
        .append(&sub1, &[entry(json!({"n": 1}))])
        .await
        .unwrap();
    store
        .append(&sub2, &[entry(json!({"n": 1}))])
        .await
        .unwrap();
    store
        .append(&other, &[entry(json!({"n": 1}))])
        .await
        .unwrap();
    store
        .append(&other_proj, &[entry(json!({"n": 1}))])
        .await
        .unwrap();

    store.delete(&key).await.expect("delete cascade");

    assert!(store.load(&key).await.unwrap().is_none());
    assert!(store.load(&sub1).await.unwrap().is_none());
    assert!(store.load(&sub2).await.unwrap().is_none());
    let other_loaded = store.load(&other).await.unwrap().expect("other preserved");
    assert_eq!(
        other_loaded.len(),
        1,
        "contract 10: other session preserved"
    );
    let other_proj_loaded = store
        .load(&other_proj)
        .await
        .unwrap()
        .expect("other-proj preserved");
    assert_eq!(
        other_proj_loaded.len(),
        1,
        "contract 10: other-proj preserved"
    );
    if has_list_subkeys {
        let subkeys = store
            .list_subkeys(&SessionListSubkeysKey {
                project_key: key.project_key.clone(),
                session_id: key.session_id.clone(),
            })
            .await
            .expect("list_subkeys");
        assert!(subkeys.is_empty(), "contract 10: subkeys cleared");
    }
    if has_list_sessions {
        let listed = store.list_sessions(&key.project_key).await.expect("list");
        assert!(
            !listed.iter().any(|s| s.session_id == key.session_id),
            "contract 10: deleted session not in list"
        );
    }

    // 11. delete with subpath removes only that subkey.
    let store = fresh();
    store.append(&key, &[entry(json!({"n": 1}))]).await.unwrap();
    store
        .append(&sub1, &[entry(json!({"n": 1}))])
        .await
        .unwrap();
    store
        .append(&sub2, &[entry(json!({"n": 1}))])
        .await
        .unwrap();

    store.delete(&sub1).await.expect("delete sub1");

    assert!(store.load(&sub1).await.unwrap().is_none());
    let sub2_loaded = store.load(&sub2).await.unwrap().expect("sub2 preserved");
    assert_eq!(sub2_loaded.len(), 1);
    let main_loaded = store.load(&key).await.unwrap().expect("main preserved");
    assert_eq!(main_loaded.len(), 1);
    if has_list_subkeys {
        let subkeys = store
            .list_subkeys(&SessionListSubkeysKey {
                project_key: key.project_key.clone(),
                session_id: key.session_id.clone(),
            })
            .await
            .expect("list_subkeys");
        assert_eq!(
            subkeys,
            vec!["subagents/agent-2".to_string()],
            "contract 11: only non-deleted subpath remains"
        );
    }
}

// ---------------------------------------------------------------------
// Optional: list_subkeys (2 contracts)
// ---------------------------------------------------------------------

async fn optional_list_subkeys<F>(fresh: &F)
where
    F: Fn() -> Arc<dyn SessionStore>,
{
    // 12. list_subkeys returns subpaths.
    let store = fresh();
    let key = default_key();
    store.append(&key, &[entry(json!({"n": 1}))]).await.unwrap();
    let sub1 = SessionKey {
        project_key: key.project_key.clone(),
        session_id: key.session_id.clone(),
        subpath: Some("subagents/agent-1".into()),
    };
    let sub2 = SessionKey {
        project_key: key.project_key.clone(),
        session_id: key.session_id.clone(),
        subpath: Some("subagents/agent-2".into()),
    };
    let other = SessionKey {
        project_key: key.project_key.clone(),
        session_id: "other-sess".into(),
        subpath: Some("subagents/agent-x".into()),
    };
    store
        .append(&sub1, &[entry(json!({"n": 1}))])
        .await
        .unwrap();
    store
        .append(&sub2, &[entry(json!({"n": 1}))])
        .await
        .unwrap();
    store
        .append(&other, &[entry(json!({"n": 1}))])
        .await
        .unwrap();

    let mut subkeys = store
        .list_subkeys(&SessionListSubkeysKey {
            project_key: key.project_key.clone(),
            session_id: key.session_id.clone(),
        })
        .await
        .expect("list_subkeys");
    subkeys.sort();
    assert_eq!(
        subkeys,
        vec![
            "subagents/agent-1".to_string(),
            "subagents/agent-2".to_string()
        ],
        "contract 12: only own-session subkeys"
    );

    // 13. list_subkeys excludes main transcript.
    let store = fresh();
    store.append(&key, &[entry(json!({"n": 1}))]).await.unwrap();
    let subkeys = store
        .list_subkeys(&SessionListSubkeysKey {
            project_key: key.project_key.clone(),
            session_id: key.session_id.clone(),
        })
        .await
        .expect("list_subkeys");
    assert!(
        subkeys.is_empty(),
        "contract 13: main-only session has empty subkeys"
    );
    let never = store
        .list_subkeys(&SessionListSubkeysKey {
            project_key: "proj".into(),
            session_id: "never-appended".into(),
        })
        .await
        .expect("list_subkeys never");
    assert!(
        never.is_empty(),
        "contract 13: never-appended session has empty subkeys"
    );
}

// ---------------------------------------------------------------------
// Test-entry construction helpers
// ---------------------------------------------------------------------

/// Build a test entry satisfying `SessionStoreEntry` (`type` is
/// required). Adapters must treat entries as opaque pass-through
/// blobs; the value of `type` is irrelevant to the contracts.
fn entry(extras: Value) -> SessionStoreEntry {
    let timestamp = extras
        .get("timestamp")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let uuid = extras
        .get("uuid")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    SessionStoreEntry {
        ty: "x".into(),
        uuid,
        timestamp,
        extra: extras,
    }
}

fn default_key() -> SessionKey {
    SessionKey {
        project_key: "proj".into(),
        session_id: "sess".into(),
        subpath: None,
    }
}

/// Deep-equal assertion between a round-tripped entry and the
/// original extras dict. Python's contract is "deep-equal on the
/// whole entry object"; forge-sdk's `SessionStoreEntry` splits the
/// extras out into `ty` + `uuid` + `timestamp` + `extra`, so we
/// assert on the flattened JSON form.
fn assert_entry_eq(got: &SessionStoreEntry, expected_extras: &Value) {
    // Reconstruct the full entry JSON the way `entry()` built it —
    // `type` = "x" plus any uuid/timestamp that were in extras. The
    // round-trip contract: what went in is what comes out.
    let mut expected = serde_json::Map::new();
    expected.insert("type".into(), Value::String("x".into()));
    if let Some(obj) = expected_extras.as_object() {
        for (k, v) in obj {
            expected.insert(k.clone(), v.clone());
        }
    }

    let mut actual = serde_json::Map::new();
    actual.insert("type".into(), Value::String(got.ty.clone()));
    if let Some(u) = &got.uuid {
        actual.insert("uuid".into(), Value::String(u.clone()));
    }
    if let Some(t) = &got.timestamp {
        actual.insert("timestamp".into(), Value::String(t.clone()));
    }
    if let Some(obj) = got.extra.as_object() {
        for (k, v) in obj {
            actual.insert(k.clone(), v.clone());
        }
    }

    assert_eq!(
        Value::Object(actual.clone()),
        Value::Object(expected.clone()),
        "entry round-trip mismatch"
    );
}
