//! Persistent transcript mirror for sessions.
//!
//! Mirrors Python `claude-agent-sdk` v0.1.64's `SessionStore` protocol
//! (`types.py:1169-1257`). The `claude` CLI always writes transcripts to
//! local disk; a `SessionStore` receives a secondary copy of each JSONL
//! line. Frames are batched and flushed to [`SessionStore::append`]
//! on two triggers: (a) explicit flush when a `result` message arrives,
//! and (b) eager flush when the pending buffer exceeds ~500 entries or
//! ~1 MiB.
//! At-most-once delivery — failed `append` batches drop silently (the
//! local-disk transcript is already durable).
//!
//! Two methods are required (`append`, `load`); three are optional
//! (`list_sessions`, `delete`, `list_subkeys`) with default impls returning
//! [`SessionStoreError::NotImplemented`]. Implementors override only what
//! they support. `delete` on a main-transcript key (no `subpath`) MUST
//! cascade to all subkeys.

#![allow(
    clippy::redundant_closure_for_method_calls,
    clippy::redundant_closure,
    clippy::items_after_statements
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Identifier for a session scoped under a project.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionKey {
    /// Project identifier (Python defaults to sanitised cwd).
    pub project_key: String,
    /// Session UUID.
    pub session_id: String,
    /// Optional subpath — present for subagent transcripts
    /// (e.g. `"subagents/agent-xyz"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
}

/// Key shape for `list_subkeys` — no `subpath` field. Mirrors Python
/// `SessionListSubkeysKey` (`types.py:1249`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionListSubkeysKey {
    /// Project identifier.
    pub project_key: String,
    /// Session UUID.
    pub session_id: String,
}

/// One JSONL line from a session transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStoreEntry {
    /// Line type (e.g. `"user"`, `"assistant"`, `"system"`, `"result"`).
    #[serde(rename = "type")]
    pub ty: String,
    /// SDK-assigned UUID for this entry when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// ISO-8601 timestamp when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// Every other field present on the wire.
    #[serde(flatten)]
    pub extra: Value,
}

/// One row in the `list_sessions` result. Wire field name is `mtime`
/// (milliseconds since Unix epoch) per Python `types.py:1153-1159`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStoreListEntry {
    /// Session UUID.
    pub session_id: String,
    /// Last-modification time in milliseconds since Unix epoch.
    pub mtime: u64,
}

/// Transcript-mirror adapter. Mirrors Python's `SessionStore` protocol.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Append one batch of transcript lines to the session identified by
    /// `key`. Batches arrive from the SDK on result/flush boundaries
    /// (see module docs for exact flush triggers).
    async fn append(
        &self,
        key: &SessionKey,
        entries: &[SessionStoreEntry],
    ) -> Result<(), SessionStoreError>;

    /// Load all entries for a session (used once at session resume). Returns
    /// `None` when no entries are stored for this key.
    async fn load(
        &self,
        key: &SessionKey,
    ) -> Result<Option<Vec<SessionStoreEntry>>, SessionStoreError>;

    /// List sessions under `project_key`, most-recent first (by `mtime`).
    async fn list_sessions(
        &self,
        project_key: &str,
    ) -> Result<Vec<SessionStoreListEntry>, SessionStoreError> {
        let _ = project_key;
        Err(SessionStoreError::NotImplemented)
    }

    /// Advisory — returns `true` when this implementation overrides
    /// [`list_sessions`](Self::list_sessions). The pre-flight validator in
    /// [`Client::spawn`](crate::Client::spawn) uses this to refuse
    /// `continue_conversation` combinations that would later fail with
    /// [`SessionStoreError::NotImplemented`] mid-session. Custom stores
    /// that provide `list_sessions` should override to `true`. Mirrors
    /// Python's reflection-based check at
    /// `_internal/session_store_validation.py:9-17`.
    fn provides_list_sessions(&self) -> bool {
        false
    }

    /// Delete a session. When `key.subpath` is `None`, implementations
    /// MUST cascade and delete all subkeys under the same `session_id`.
    async fn delete(&self, key: &SessionKey) -> Result<(), SessionStoreError> {
        let _ = key;
        Err(SessionStoreError::NotImplemented)
    }

    /// List subkey strings under a session (subagent transcripts). Note
    /// the key type — [`SessionListSubkeysKey`] has no `subpath`, matching
    /// Python's `SessionListSubkeysKey` type.
    async fn list_subkeys(
        &self,
        key: &SessionListSubkeysKey,
    ) -> Result<Vec<String>, SessionStoreError> {
        let _ = key;
        Err(SessionStoreError::NotImplemented)
    }
}

/// Error surface for [`SessionStore`].
#[derive(Debug, thiserror::Error)]
pub enum SessionStoreError {
    /// Backend I/O failure.
    #[error("session store I/O error: {0}")]
    Io(String),
    /// Backend-specific failure with details.
    #[error("session store error: {0}")]
    Backend(String),
    /// Optional trait method not implemented by this backend.
    #[error("session store method not implemented")]
    NotImplemented,
}

impl From<std::io::Error> for SessionStoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// In-memory [`SessionStore`] — useful for tests and ephemeral sessions.
#[derive(Debug, Default)]
pub struct MemorySessionStore {
    inner: Mutex<HashMap<SessionKey, Vec<SessionStoreEntry>>>,
    mtimes: Mutex<HashMap<(String, String), u64>>,
}

impl MemorySessionStore {
    /// Construct an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn touch_mtime(&self, project_key: &str, session_id: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        let mut mtimes = self
            .mtimes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        mtimes.insert((project_key.to_string(), session_id.to_string()), now);
    }
}

#[async_trait]
impl SessionStore for MemorySessionStore {
    fn provides_list_sessions(&self) -> bool {
        true
    }

    async fn append(
        &self,
        key: &SessionKey,
        entries: &[SessionStoreEntry],
    ) -> Result<(), SessionStoreError> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner
            .entry(key.clone())
            .or_default()
            .extend_from_slice(entries);
        drop(inner);
        self.touch_mtime(&key.project_key, &key.session_id);
        Ok(())
    }

    async fn load(
        &self,
        key: &SessionKey,
    ) -> Result<Option<Vec<SessionStoreEntry>>, SessionStoreError> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(inner.get(key).cloned())
    }

    async fn list_sessions(
        &self,
        project_key: &str,
    ) -> Result<Vec<SessionStoreListEntry>, SessionStoreError> {
        let mtimes = self
            .mtimes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut rows: Vec<SessionStoreListEntry> = mtimes
            .iter()
            .filter(|((pk, _), _)| pk == project_key)
            .map(|((_, sid), m)| SessionStoreListEntry {
                session_id: sid.clone(),
                mtime: *m,
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.mtime));
        Ok(rows)
    }

    async fn delete(&self, key: &SessionKey) -> Result<(), SessionStoreError> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if key.subpath.is_none() {
            // Cascade: remove main + all subkeys under same session_id.
            inner.retain(|k, _| {
                !(k.project_key == key.project_key && k.session_id == key.session_id)
            });
            let mut mtimes = self
                .mtimes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            mtimes.remove(&(key.project_key.clone(), key.session_id.clone()));
        } else {
            inner.remove(key);
        }
        Ok(())
    }

    async fn list_subkeys(
        &self,
        key: &SessionListSubkeysKey,
    ) -> Result<Vec<String>, SessionStoreError> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Return sanitised subkey names to match FsSessionStore's
        // on-disk naming. Callers should treat subkey strings as
        // opaque identifiers keyed by the same adapter.
        let mut subkeys: Vec<String> = inner
            .keys()
            .filter(|k| k.project_key == key.project_key && k.session_id == key.session_id)
            .filter_map(|k| k.subpath.as_deref().map(sanitise))
            .collect();
        subkeys.sort();
        Ok(subkeys)
    }
}

/// Filesystem [`SessionStore`] — mirrors entries to JSONL files under
/// `<root>/<project_key>/<session_id>[/<subpath>].jsonl`.
#[derive(Debug)]
pub struct FsSessionStore {
    root: PathBuf,
}

impl FsSessionStore {
    /// Construct a filesystem-backed store rooted at `root`. Creates the
    /// directory if missing.
    ///
    /// # Errors
    ///
    /// [`SessionStoreError::Io`] when the directory can't be created.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, SessionStoreError> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn session_path(&self, key: &SessionKey) -> PathBuf {
        let mut p = self.root.clone();
        p.push(sanitise(&key.project_key));
        if let Some(sub) = &key.subpath {
            p.push(&key.session_id);
            p.push(format!("{}.jsonl", sanitise(sub)));
        } else {
            p.push(format!("{}.jsonl", &key.session_id));
        }
        p
    }

    fn project_dir(&self, project_key: &str) -> PathBuf {
        let mut p = self.root.clone();
        p.push(sanitise(project_key));
        p
    }

    fn session_dir(&self, project_key: &str, session_id: &str) -> PathBuf {
        let mut p = self.project_dir(project_key);
        p.push(session_id);
        p
    }
}

/// Python-SDK-compatible path sanitisation. Delegates to
/// [`crate::sessions::sanitize_path_public`] — single authoritative
/// implementation for on-disk project-key layout.
pub(crate) fn sanitise(s: &str) -> String {
    crate::sessions::sanitize_path_public(s)
}

/// Derive a [`SessionKey`] from an absolute transcript file path relative
/// to the projects root. Main transcripts live at
/// `<projects_dir>/<project_key>/<session_id>.jsonl`; subagent
/// transcripts at `<projects_dir>/<project_key>/<session_id>/subagents/<...>.jsonl`.
/// Returns `None` for paths outside `projects_dir` or with unrecognised
/// shape. Mirrors Python `_internal/session_store.py::file_path_to_session_key`.
#[must_use]
pub fn file_path_to_session_key(file_path: &str, projects_dir: &str) -> Option<SessionKey> {
    use std::path::Path;

    let file = Path::new(file_path);
    let projects = Path::new(projects_dir);
    let rel = file.strip_prefix(projects).ok()?;
    let mut parts: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
    if parts.len() < 2 {
        return None;
    }
    let project_key = parts.remove(0).to_string();
    let first = parts[0];

    // Main transcript: <project_key>/<session_id>.jsonl
    if parts.len() == 1 {
        if let Some(session_id) = strip_jsonl_suffix(first) {
            return Some(SessionKey {
                project_key,
                session_id: session_id.to_string(),
                subpath: None,
            });
        }
        return None;
    }

    // Subagent: <project_key>/<session_id>/<...>.jsonl
    if parts.len() >= 3 {
        let session_id = parts.remove(0).to_string();
        let last_idx = parts.len() - 1;
        if let Some(stripped) = strip_jsonl_suffix(parts[last_idx]) {
            parts[last_idx] = stripped;
        }
        let subpath = parts.join("/");
        return Some(SessionKey {
            project_key,
            session_id,
            subpath: Some(subpath),
        });
    }

    None
}

/// Case-insensitive `.jsonl` suffix stripper. Returns `None` when the
/// name doesn't end in `.jsonl`/`.JSONL`/any casing. Strips a single
/// occurrence, avoiding `trim_end_matches` which would peel multiple
/// suffixes from `foo.jsonl.jsonl`.
fn strip_jsonl_suffix(name: &str) -> Option<&str> {
    let lc = name.to_ascii_lowercase();
    let stem_len = lc.strip_suffix(".jsonl")?.len();
    name.get(..stem_len)
}

#[async_trait]
impl SessionStore for FsSessionStore {
    fn provides_list_sessions(&self) -> bool {
        true
    }

    async fn append(
        &self,
        key: &SessionKey,
        entries: &[SessionStoreEntry],
    ) -> Result<(), SessionStoreError> {
        let path = self.session_path(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut buf = Vec::with_capacity(entries.len() * 128);
        for e in entries {
            let line = serde_json::to_string(e).map_err(|e| {
                SessionStoreError::Backend(format!("serialise transcript entry: {e}"))
            })?;
            buf.extend_from_slice(line.as_bytes());
            buf.push(b'\n');
        }
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .await?;
        f.write_all(&buf).await?;
        f.flush().await?;
        Ok(())
    }

    async fn load(
        &self,
        key: &SessionKey,
    ) -> Result<Option<Vec<SessionStoreEntry>>, SessionStoreError> {
        let path = self.session_path(key);
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(SessionStoreError::from(e)),
        };
        let mut out = Vec::new();
        for (idx, line) in text.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            let entry: SessionStoreEntry = serde_json::from_str(line)
                .map_err(|e| SessionStoreError::Backend(format!("parse line {}: {e}", idx + 1)))?;
            out.push(entry);
        }
        Ok(Some(out))
    }

    async fn list_sessions(
        &self,
        project_key: &str,
    ) -> Result<Vec<SessionStoreListEntry>, SessionStoreError> {
        let dir = self.project_dir(project_key);
        let mut read = match tokio::fs::read_dir(&dir).await {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(SessionStoreError::from(e)),
        };
        let mut out = Vec::new();
        while let Some(entry) = read.next_entry().await? {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(session_id) = name.strip_suffix(".jsonl") else {
                continue;
            };
            let meta = entry.metadata().await?;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
            out.push(SessionStoreListEntry {
                session_id: session_id.to_string(),
                mtime,
            });
        }
        out.sort_by_key(|r| std::cmp::Reverse(r.mtime));
        Ok(out)
    }

    async fn delete(&self, key: &SessionKey) -> Result<(), SessionStoreError> {
        // Remove-if-present semantics: NotFound is fine, but all other
        // errors (permission denied, interrupted syscall) must propagate
        // so the caller doesn't think the delete succeeded when it didn't.
        async fn remove_file_if_exists(path: &Path) -> Result<(), SessionStoreError> {
            match tokio::fs::remove_file(path).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(SessionStoreError::from(e)),
            }
        }
        async fn remove_dir_if_exists(path: &Path) -> Result<(), SessionStoreError> {
            match tokio::fs::remove_dir_all(path).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(SessionStoreError::from(e)),
            }
        }
        if key.subpath.is_none() {
            remove_file_if_exists(&self.session_path(key)).await?;
            remove_dir_if_exists(&self.session_dir(&key.project_key, &key.session_id)).await?;
        } else {
            remove_file_if_exists(&self.session_path(key)).await?;
        }
        Ok(())
    }

    async fn list_subkeys(
        &self,
        key: &SessionListSubkeysKey,
    ) -> Result<Vec<String>, SessionStoreError> {
        let dir = self.session_dir(&key.project_key, &key.session_id);
        let mut read = match tokio::fs::read_dir(&dir).await {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(SessionStoreError::from(e)),
        };
        let mut out = Vec::new();
        while let Some(entry) = read.next_entry().await? {
            if let Some(name) = entry.file_name().to_str() {
                if let Some(sub) = name.strip_suffix(".jsonl") {
                    out.push(sub.to_string());
                }
            }
        }
        out.sort();
        Ok(out)
    }
}
