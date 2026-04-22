//! Tests for the in-memory `SessionStore`.
//! Contract mirrors Python v0.1.64 `SessionStore` protocol.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::session::store::{
    MemorySessionStore, SessionKey, SessionListSubkeysKey, SessionStore, SessionStoreEntry,
};
use serde_json::json;

fn key(session: &str) -> SessionKey {
    SessionKey {
        project_key: "proj".into(),
        session_id: session.into(),
        subpath: None,
    }
}

fn sub(session: &str, sub: &str) -> SessionKey {
    SessionKey {
        project_key: "proj".into(),
        session_id: session.into(),
        subpath: Some(sub.into()),
    }
}

fn entry(ty: &str, body: serde_json::Value) -> SessionStoreEntry {
    SessionStoreEntry {
        ty: ty.into(),
        uuid: None,
        timestamp: None,
        extra: body,
    }
}

#[tokio::test]
async fn append_then_load_returns_entries_in_order() {
    let store = MemorySessionStore::new();
    let k = key("s1");
    store
        .append(&k, &[entry("user", json!({"content": "hi"}))])
        .await
        .expect("append");
    store
        .append(&k, &[entry("assistant", json!({"content": "hello"}))])
        .await
        .expect("append");

    let loaded = store.load(&k).await.expect("load").expect("present");
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].ty, "user");
    assert_eq!(loaded[1].ty, "assistant");
}

#[tokio::test]
async fn load_missing_returns_none() {
    let store = MemorySessionStore::new();
    assert!(store.load(&key("nope")).await.expect("load").is_none());
}

#[tokio::test]
async fn delete_removes_session() {
    let store = MemorySessionStore::new();
    let k = key("s2");
    store.append(&k, &[entry("user", json!({}))]).await.unwrap();
    store.delete(&k).await.unwrap();
    assert!(store.load(&k).await.unwrap().is_none());
}

#[tokio::test]
async fn delete_main_cascades_to_subkeys() {
    let store = MemorySessionStore::new();
    let main = key("s3");
    let sk1 = sub("s3", "agent-a");
    let sk2 = sub("s3", "agent-b");
    store
        .append(&main, &[entry("user", json!({}))])
        .await
        .unwrap();
    store
        .append(&sk1, &[entry("user", json!({}))])
        .await
        .unwrap();
    store
        .append(&sk2, &[entry("user", json!({}))])
        .await
        .unwrap();

    store.delete(&main).await.unwrap();
    assert!(store.load(&main).await.unwrap().is_none());
    assert!(store.load(&sk1).await.unwrap().is_none());
    assert!(store.load(&sk2).await.unwrap().is_none());
}

#[tokio::test]
async fn list_sessions_under_project() {
    let store = MemorySessionStore::new();
    store
        .append(&key("s4"), &[entry("user", json!({}))])
        .await
        .unwrap();
    store
        .append(&key("s5"), &[entry("user", json!({}))])
        .await
        .unwrap();
    let rows = store.list_sessions("proj").await.expect("list");
    assert_eq!(rows.len(), 2);
    let ids: Vec<_> = rows.iter().map(|r| r.session_id.as_str()).collect();
    assert!(ids.contains(&"s4"));
    assert!(ids.contains(&"s5"));
}

#[tokio::test]
async fn list_subkeys_returns_subpaths() {
    let store = MemorySessionStore::new();
    store
        .append(&sub("s6", "alpha"), &[entry("user", json!({}))])
        .await
        .unwrap();
    store
        .append(&sub("s6", "beta"), &[entry("user", json!({}))])
        .await
        .unwrap();
    let subkeys = store
        .list_subkeys(&SessionListSubkeysKey {
            project_key: "proj".into(),
            session_id: "s6".into(),
        })
        .await
        .expect("list_subkeys");
    assert_eq!(subkeys, vec!["alpha".to_string(), "beta".to_string()]);
}
