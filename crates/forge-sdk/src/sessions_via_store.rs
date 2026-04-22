//! `SessionStore`-backed async variants of [`sessions`](crate::sessions)
//! and [`session_mutations`](crate::session_mutations).
//!
//! Each function here delegates to the [`SessionStore`] protocol rather
//! than the local filesystem. Mirrors the `*_from_store` / `*_via_store`
//! helpers in Python SDK's `_internal/sessions.py` +
//! `_internal/session_mutations.py`.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::error::Error;
use crate::public_types::{SDKSessionInfo, SessionMessage, SessionMessageKind};
use crate::session_store::{
    SessionKey, SessionListSubkeysKey, SessionStore, SessionStoreEntry, SessionStoreListEntry,
};

/// List sessions for `project_key` via the attached [`SessionStore`].
/// Mirrors Python `list_sessions_from_store`.
///
/// # Errors
///
/// Any error the adapter raises from `SessionStore::list_sessions`.
pub async fn list_sessions_from_store(
    store: Arc<dyn SessionStore>,
    project_key: &str,
) -> Result<Vec<SessionStoreListEntry>, Error> {
    Ok(store.list_sessions(project_key).await?)
}

/// Load metadata for one session via the attached store. Returns `None`
/// when the store has no entries for the key.
///
/// # Errors
///
/// Any error from `SessionStore::load`.
pub async fn get_session_info_from_store(
    store: Arc<dyn SessionStore>,
    project_key: &str,
    session_id: &str,
) -> Result<Option<SDKSessionInfo>, Error> {
    let key = SessionKey {
        project_key: project_key.into(),
        session_id: session_id.into(),
        subpath: None,
    };
    let Some(entries) = store.load(&key).await? else {
        return Ok(None);
    };
    Ok(Some(sdk_session_info_from_entries(session_id, &entries)))
}

/// Read the transcript for one session via the store.
///
/// # Errors
///
/// Any error from `SessionStore::load`.
pub async fn get_session_messages_from_store(
    store: Arc<dyn SessionStore>,
    project_key: &str,
    session_id: &str,
) -> Result<Vec<SessionMessage>, Error> {
    let key = SessionKey {
        project_key: project_key.into(),
        session_id: session_id.into(),
        subpath: None,
    };
    let Some(entries) = store.load(&key).await? else {
        return Ok(Vec::new());
    };
    Ok(entries.iter().filter_map(to_session_message).collect())
}

/// List subagent ids for a session via the store.
///
/// # Errors
///
/// Any error from `SessionStore::list_subkeys`.
pub async fn list_subagents_from_store(
    store: Arc<dyn SessionStore>,
    project_key: &str,
    session_id: &str,
) -> Result<Vec<String>, Error> {
    let key = SessionListSubkeysKey {
        project_key: project_key.into(),
        session_id: session_id.into(),
    };
    Ok(store.list_subkeys(&key).await?)
}

/// Read a subagent transcript via the store. Returns an empty Vec when
/// the store has no entries for that subkey.
///
/// # Errors
///
/// Any error from `SessionStore::load`.
pub async fn get_subagent_messages_from_store(
    store: Arc<dyn SessionStore>,
    project_key: &str,
    session_id: &str,
    subagent_id: &str,
) -> Result<Vec<SessionMessage>, Error> {
    let key = SessionKey {
        project_key: project_key.into(),
        session_id: session_id.into(),
        subpath: Some(format!("subagents/{subagent_id}")),
    };
    let Some(entries) = store.load(&key).await? else {
        return Ok(Vec::new());
    };
    Ok(entries.iter().filter_map(to_session_message).collect())
}

/// Rename a session via the store — appends a `custom-title` entry.
///
/// # Errors
///
/// Any error from `SessionStore::append` plus `MessageParse` for
/// empty `title`.
pub async fn rename_session_via_store(
    store: Arc<dyn SessionStore>,
    project_key: &str,
    session_id: &str,
    title: &str,
) -> Result<(), Error> {
    let stripped = title.trim();
    if stripped.is_empty() {
        return Err(Error::MessageParse {
            reason: "title must be non-empty".into(),
            data: None,
        });
    }
    let entry = entry_from_payload(json!({
        "type": "custom-title",
        "customTitle": stripped,
        "sessionId": session_id,
    }))?;
    let key = SessionKey {
        project_key: project_key.into(),
        session_id: session_id.into(),
        subpath: None,
    };
    store.append(&key, &[entry]).await?;
    Ok(())
}

/// Tag a session via the store. Pass `None` to clear.
///
/// # Errors
///
/// `MessageParse` for empty tag; any `SessionStore::append` error.
pub async fn tag_session_via_store(
    store: Arc<dyn SessionStore>,
    project_key: &str,
    session_id: &str,
    tag: Option<&str>,
) -> Result<(), Error> {
    let stored: String = match tag {
        None => String::new(),
        Some(raw) => {
            let stripped = raw.trim();
            if stripped.is_empty() {
                return Err(Error::MessageParse {
                    reason: "tag must be non-empty (use None to clear)".into(),
                    data: None,
                });
            }
            stripped.to_string()
        }
    };
    let entry = entry_from_payload(json!({
        "type": "tag",
        "tag": stored,
        "sessionId": session_id,
    }))?;
    let key = SessionKey {
        project_key: project_key.into(),
        session_id: session_id.into(),
        subpath: None,
    };
    store.append(&key, &[entry]).await?;
    Ok(())
}

/// Delete a session via the store. Cascades to subagent subkeys per
/// `SessionStore::delete` contract.
///
/// # Errors
///
/// Any error from `SessionStore::delete`.
pub async fn delete_session_via_store(
    store: Arc<dyn SessionStore>,
    project_key: &str,
    session_id: &str,
) -> Result<(), Error> {
    let key = SessionKey {
        project_key: project_key.into(),
        session_id: session_id.into(),
        subpath: None,
    };
    store.delete(&key).await?;
    Ok(())
}

/// Fork a session via the attached store. Loads the source session,
/// remaps UUIDs, and appends the forked entries into a new session id.
/// Mirrors Python `fork_session_via_store`.
///
/// # Errors
///
/// - [`Error::MessageParse`] when `session_id` or `up_to_message_id`
///   aren't valid UUIDs, when the source session is empty, or when the
///   boundary can't be found.
/// - Any error from `SessionStore::load` / `append`.
pub async fn fork_session_via_store(
    store: Arc<dyn SessionStore>,
    project_key: &str,
    session_id: &str,
    up_to_message_id: Option<&str>,
    title: Option<&str>,
) -> Result<crate::session_mutations::ForkSessionResult, Error> {
    crate::session_mutations::validate_uuid_public(session_id)?;
    if let Some(m) = up_to_message_id {
        crate::session_mutations::validate_uuid_public(m)?;
    }
    let source_key = SessionKey {
        project_key: project_key.into(),
        session_id: session_id.into(),
        subpath: None,
    };
    let entries = store
        .load(&source_key)
        .await?
        .ok_or_else(|| Error::MessageParse {
            reason: format!("session {session_id} has no messages to fork"),
            data: None,
        })?;

    let new_session_id = uuid::Uuid::new_v4().to_string();
    // Pass 1 — mint new UUIDs for every entry up-front so parentUuid
    // references always find a mapping.
    let mut uuid_remap: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for entry in &entries {
        if let Some(old) = entry.uuid.as_deref() {
            uuid_remap
                .entry(old.to_string())
                .or_insert_with(|| uuid::Uuid::new_v4().to_string());
        }
    }

    // Pass 2 — rewrite each entry using the fully-populated map.
    let mut remapped: Vec<SessionStoreEntry> = Vec::new();
    let mut saw_boundary = false;

    for entry in &entries {
        let mut value = serde_json::to_value(entry).map_err(|e| Error::MessageParse {
            reason: format!("encode entry: {e}"),
            data: None,
        })?;
        let boundary_hit = crate::session_mutations::remap_entry_fields(
            &mut value,
            &uuid_remap,
            &new_session_id,
            up_to_message_id,
        );
        let new_entry: SessionStoreEntry =
            serde_json::from_value(value).map_err(|e| Error::MessageParse {
                reason: format!("decode remapped entry: {e}"),
                data: None,
            })?;
        remapped.push(new_entry);
        if boundary_hit {
            saw_boundary = true;
            break;
        }
    }

    if remapped.is_empty() {
        return Err(Error::MessageParse {
            reason: format!("session {session_id} has no messages to fork"),
            data: None,
        });
    }
    if up_to_message_id.is_some() && !saw_boundary {
        return Err(Error::MessageParse {
            reason: format!(
                "up_to_message_id {} not found in transcript",
                up_to_message_id.unwrap_or("")
            ),
            data: None,
        });
    }

    if let Some(t) = title {
        let stripped = t.trim();
        if !stripped.is_empty() {
            let payload = json!({
                "type": "custom-title",
                "customTitle": stripped,
                "sessionId": new_session_id,
            });
            let entry: SessionStoreEntry =
                serde_json::from_value(payload).map_err(|e| Error::MessageParse {
                    reason: format!("encode fork title: {e}"),
                    data: None,
                })?;
            remapped.push(entry);
        }
    }

    let dest_key = SessionKey {
        project_key: project_key.into(),
        session_id: new_session_id.clone(),
        subpath: None,
    };
    store.append(&dest_key, &remapped).await?;
    Ok(crate::session_mutations::ForkSessionResult {
        session_id: new_session_id,
    })
}

fn to_session_message(entry: &SessionStoreEntry) -> Option<SessionMessage> {
    let kind = match entry.ty.as_str() {
        "user" => SessionMessageKind::User,
        "assistant" => SessionMessageKind::Assistant,
        _ => return None,
    };
    if entry
        .extra
        .get("parent_tool_use_id")
        .is_some_and(|v| !v.is_null())
    {
        return None;
    }
    let uuid = entry.uuid.clone().unwrap_or_default();
    let session_id = entry
        .extra
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let message = entry.extra.get("message").cloned().unwrap_or(Value::Null);
    Some(SessionMessage {
        kind,
        uuid,
        session_id,
        message,
        parent_tool_use_id: None,
    })
}

fn sdk_session_info_from_entries(
    session_id: &str,
    entries: &[SessionStoreEntry],
) -> SDKSessionInfo {
    let mut first_prompt: Option<String> = None;
    let mut custom_title: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut tag: Option<String> = None;
    let mut summary: Option<String> = None;
    for entry in entries {
        if first_prompt.is_none()
            && entry.ty == "user"
            && entry
                .extra
                .get("parent_tool_use_id")
                .is_none_or(Value::is_null)
        {
            if let Some(content) = entry
                .extra
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
            {
                first_prompt = Some(content.to_string());
            }
        }
        // Mutable metadata — LAST-wins. `rename_session_via_store` and
        // `tag_session_via_store` append new entries; most recent is truth.
        if let Some(v) = entry.extra.get("customTitle").and_then(Value::as_str) {
            custom_title = Some(v.to_string());
        }
        if let Some(v) = entry.extra.get("cwd").and_then(Value::as_str) {
            cwd = Some(v.to_string());
        }
        if let Some(v) = entry.extra.get("gitBranch").and_then(Value::as_str) {
            git_branch = Some(v.to_string());
        }
        if let Some(v) = entry.extra.get("tag").and_then(Value::as_str) {
            tag = if v.is_empty() {
                None
            } else {
                Some(v.to_string())
            };
        }
        if let Some(v) = entry.extra.get("summary").and_then(Value::as_str) {
            summary = Some(v.to_string());
        }
    }
    let display = summary
        .or_else(|| custom_title.clone())
        .or_else(|| first_prompt.clone())
        .unwrap_or_default();
    SDKSessionInfo {
        session_id: session_id.into(),
        summary: display,
        last_modified: 0,
        file_size: None,
        custom_title,
        first_prompt,
        git_branch,
        cwd,
        tag,
        created_at: None,
    }
}

fn entry_from_payload(payload: Value) -> Result<SessionStoreEntry, Error> {
    serde_json::from_value(payload).map_err(|e| Error::MessageParse {
        reason: format!("encode mutation payload: {e}"),
        data: None,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::session_store::MemorySessionStore;

    fn entry(kind: &str, content: &str) -> SessionStoreEntry {
        SessionStoreEntry {
            ty: kind.into(),
            uuid: None,
            timestamp: None,
            extra: json!({"message": {"content": content}}),
        }
    }

    #[tokio::test]
    async fn empty_store_returns_none_for_info() {
        let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
        let r = get_session_info_from_store(store, "proj", "sess")
            .await
            .unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn rename_then_get_messages_round_trips() {
        let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
        let key = SessionKey {
            project_key: "proj".into(),
            session_id: "550e8400-e29b-41d4-a716-446655440000".into(),
            subpath: None,
        };
        // Seed the store with one user message.
        store
            .append(&key, &[entry("user", "hello world")])
            .await
            .unwrap();
        rename_session_via_store(
            store.clone(),
            &key.project_key,
            &key.session_id,
            "Test title",
        )
        .await
        .unwrap();
        let info = get_session_info_from_store(store.clone(), &key.project_key, &key.session_id)
            .await
            .unwrap()
            .expect("present");
        assert_eq!(info.custom_title.as_deref(), Some("Test title"));
        let msgs = get_session_messages_from_store(store, &key.project_key, &key.session_id)
            .await
            .unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].kind, SessionMessageKind::User);
    }

    #[tokio::test]
    async fn delete_via_store_clears_entries() {
        let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
        let key = SessionKey {
            project_key: "proj".into(),
            session_id: "550e8400-e29b-41d4-a716-446655440001".into(),
            subpath: None,
        };
        store.append(&key, &[entry("user", "x")]).await.unwrap();
        delete_session_via_store(store.clone(), &key.project_key, &key.session_id)
            .await
            .unwrap();
        let loaded = store.load(&key).await.unwrap();
        assert!(loaded.is_none());
    }
}
