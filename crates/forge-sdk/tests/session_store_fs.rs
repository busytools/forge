//! Tests for the filesystem-backed `SessionStore`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::session_store::{
    FsSessionStore, SessionKey, SessionListSubkeysKey, SessionStore, SessionStoreEntry,
};
use serde_json::json;
use tempfile::tempdir;

fn entry(ty: &str, body: serde_json::Value) -> SessionStoreEntry {
    SessionStoreEntry {
        ty: ty.into(),
        uuid: None,
        timestamp: None,
        extra: body,
    }
}

#[tokio::test]
async fn append_then_load_persists() {
    let tmp = tempdir().unwrap();
    let store = FsSessionStore::new(tmp.path()).expect("new");
    let k = SessionKey {
        project_key: "proj".into(),
        session_id: "s1".into(),
        subpath: None,
    };
    store
        .append(&k, &[entry("user", json!({"content": "hi"}))])
        .await
        .expect("append");
    let loaded = store.load(&k).await.expect("load").expect("present");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].ty, "user");
}

#[tokio::test]
async fn load_missing_returns_none() {
    let tmp = tempdir().unwrap();
    let store = FsSessionStore::new(tmp.path()).expect("new");
    let k = SessionKey {
        project_key: "proj".into(),
        session_id: "nope".into(),
        subpath: None,
    };
    assert!(store.load(&k).await.expect("load").is_none());
}

#[tokio::test]
async fn delete_cascades_to_subkeys() {
    let tmp = tempdir().unwrap();
    let store = FsSessionStore::new(tmp.path()).expect("new");
    let main = SessionKey {
        project_key: "proj".into(),
        session_id: "s2".into(),
        subpath: None,
    };
    let sk = SessionKey {
        project_key: "proj".into(),
        session_id: "s2".into(),
        subpath: Some("agent-a".into()),
    };

    store
        .append(&main, &[entry("user", json!({}))])
        .await
        .unwrap();
    store
        .append(&sk, &[entry("user", json!({}))])
        .await
        .unwrap();

    store.delete(&main).await.unwrap();
    assert!(store.load(&main).await.unwrap().is_none());
    assert!(store.load(&sk).await.unwrap().is_none());
}

#[tokio::test]
async fn list_subkeys_enumerates_subagents() {
    let tmp = tempdir().unwrap();
    let store = FsSessionStore::new(tmp.path()).expect("new");
    for name in ["alpha", "beta", "gamma"] {
        let sk = SessionKey {
            project_key: "proj".into(),
            session_id: "s3".into(),
            subpath: Some(name.into()),
        };
        store
            .append(&sk, &[entry("user", json!({}))])
            .await
            .unwrap();
    }
    let subkeys = store
        .list_subkeys(&SessionListSubkeysKey {
            project_key: "proj".into(),
            session_id: "s3".into(),
        })
        .await
        .unwrap();
    assert_eq!(subkeys, vec!["alpha", "beta", "gamma"]);
}
