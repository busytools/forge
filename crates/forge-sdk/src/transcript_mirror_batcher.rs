//! Coalescing adapter between `transcript_mirror` frames and a
//! [`SessionStore`](crate::session_store::SessionStore).
//!
//! The CLI emits `{"type":"transcript_mirror","filePath":...,"entries":[...]}`
//! frames alongside regular stream-json output. The client peels them off
//! and hands them to [`TranscriptMirrorBatcher::enqueue`], which accumulates
//! them and flushes to `store.append` either on an explicit `flush()` call
//! (on `result` arrival / stream end) or when the pending buffer exceeds
//! configured thresholds (eager background flush).
//!
//! Ported from Python SDK v0.1.64 `_internal/transcript_mirror_batcher.py`.
//! Adapter failures are reported via a [`Message::MirrorError`] emitted on
//! the sink channel; the failed batch is dropped (at-most-once semantics,
//! matching Python). Failures never raise — the on-disk transcript is
//! already durable, so the session continues unaffected.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error, warn};

use crate::messages::Message;
use crate::session_store::{SessionKey, SessionStore, SessionStoreEntry, file_path_to_session_key};

/// Eager-flush threshold on total entry count. Mirrors Python's
/// `MAX_PENDING_ENTRIES = 500` (`transcript_mirror_batcher.py:26`).
pub(crate) const MAX_PENDING_ENTRIES: usize = 500;

/// Eager-flush threshold on approximate buffered bytes. Mirrors Python's
/// `MAX_PENDING_BYTES = 1 << 20` (1 MiB, `transcript_mirror_batcher.py:27`).
pub(crate) const MAX_PENDING_BYTES: usize = 1 << 20;

/// Upper bound on how long a single `store.append` may run. Mirrors Python's
/// `SEND_TIMEOUT_SECONDS = 60.0` (`transcript_mirror_batcher.py:28`).
pub(crate) const SEND_TIMEOUT_SECONDS: u64 = 60;

struct PendingEntry {
    file_path: String,
    entries: Vec<SessionStoreEntry>,
}

#[derive(Default)]
struct PendingBuffer {
    items: Vec<PendingEntry>,
    entries_count: usize,
    bytes: usize,
    flush_task: Option<JoinHandle<()>>,
}

struct BatcherInner {
    store: Arc<dyn SessionStore>,
    projects_dir: String,
    error_sink: mpsc::UnboundedSender<Message>,
    send_timeout: Duration,
    max_pending_entries: usize,
    max_pending_bytes: usize,
    pending: StdMutex<PendingBuffer>,
    send_lock: TokioMutex<()>,
}

/// Coalesce `transcript_mirror` frames, flush to the attached
/// [`SessionStore`], and synthesise [`Message::MirrorError`] on append
/// failure. Cheaply cloneable — the inner state is shared via `Arc`.
#[derive(Clone)]
pub(crate) struct TranscriptMirrorBatcher {
    inner: Arc<BatcherInner>,
}

impl TranscriptMirrorBatcher {
    /// Construct with the Python-SDK defaults (500 entries / 1 MiB / 60 s).
    pub(crate) fn new(
        store: Arc<dyn SessionStore>,
        projects_dir: String,
        error_sink: mpsc::UnboundedSender<Message>,
    ) -> Self {
        Self::with_thresholds(
            store,
            projects_dir,
            error_sink,
            MAX_PENDING_ENTRIES,
            MAX_PENDING_BYTES,
            Duration::from_secs(SEND_TIMEOUT_SECONDS),
        )
    }

    /// Construct with explicit thresholds — used by tests.
    pub(crate) fn with_thresholds(
        store: Arc<dyn SessionStore>,
        projects_dir: String,
        error_sink: mpsc::UnboundedSender<Message>,
        max_pending_entries: usize,
        max_pending_bytes: usize,
        send_timeout: Duration,
    ) -> Self {
        Self {
            inner: Arc::new(BatcherInner {
                store,
                projects_dir,
                error_sink,
                send_timeout,
                max_pending_entries,
                max_pending_bytes,
                pending: StdMutex::new(PendingBuffer::default()),
                send_lock: TokioMutex::new(()),
            }),
        }
    }

    /// Buffer one frame; schedule an eager background flush if the pending
    /// buffer has grown past either threshold.
    ///
    /// Fire-and-forget; any prior in-flight flush is allowed to run to
    /// completion before the new one starts (serialised via an internal
    /// async lock so `store.append` ordering holds).
    pub(crate) fn enqueue(&self, file_path: String, entries: Vec<SessionStoreEntry>) {
        // Approximate wire size — cheaper than per-entry stringify.
        let bytes = serde_json::to_vec(&entries).map_or(0, |v| v.len());
        let entry_count = entries.len();
        let should_flush = {
            let Ok(mut buf) = self.inner.pending.lock() else {
                error!("TranscriptMirrorBatcher pending mutex poisoned; dropping frame");
                return;
            };
            buf.entries_count += entry_count;
            buf.bytes += bytes;
            buf.items.push(PendingEntry { file_path, entries });
            buf.entries_count > self.inner.max_pending_entries
                || buf.bytes > self.inner.max_pending_bytes
        };
        if should_flush {
            self.spawn_drain();
        }
    }

    /// Flush all pending entries synchronously. Awaits any in-flight eager
    /// flush first so the returned state reflects "buffer drained".
    pub(crate) async fn flush(&self) {
        let prior = {
            let Ok(mut buf) = self.inner.pending.lock() else {
                error!("TranscriptMirrorBatcher pending mutex poisoned; skipping flush");
                return;
            };
            buf.flush_task.take()
        };
        if let Some(task) = prior {
            let _ = task.await;
        }
        self.drain().await;
    }

    /// Final flush at teardown. Never raises.
    pub(crate) async fn close(&self) {
        self.flush().await;
    }

    fn spawn_drain(&self) {
        let this = self.clone();
        let task = tokio::spawn(async move {
            this.drain().await;
        });
        if let Ok(mut buf) = self.inner.pending.lock() {
            buf.flush_task = Some(task);
        } else {
            // Poisoned mutex. Abort the task so it doesn't run orphaned
            // past close() — flush() can't await a handle it can't see.
            error!("TranscriptMirrorBatcher pending mutex poisoned; aborting spawned drain");
            task.abort();
        }
    }

    async fn drain(&self) {
        let items = {
            let Ok(mut buf) = self.inner.pending.lock() else {
                error!("TranscriptMirrorBatcher pending mutex poisoned; drain aborted");
                return;
            };
            let items = std::mem::take(&mut buf.items);
            buf.entries_count = 0;
            buf.bytes = 0;
            items
        };
        if items.is_empty() {
            return;
        }
        // Serialise against any concurrent drain so `store.append` ordering
        // holds. Detaching the buffer above allows `enqueue` to keep
        // accumulating into a fresh buffer while this drain runs.
        let _guard = self.inner.send_lock.lock().await;
        self.do_flush(items).await;
    }

    async fn do_flush(&self, items: Vec<PendingEntry>) {
        let by_path = coalesce_by_path(items);
        for (file_path, entries) in by_path {
            if entries.is_empty() {
                // Avoid creating phantom keys in adapters that touch storage
                // on empty appends — nothing to write.
                continue;
            }
            let Some(key) = file_path_to_session_key(&file_path, &self.inner.projects_dir) else {
                warn!(
                    %file_path,
                    projects_dir = %self.inner.projects_dir,
                    "dropping mirror frame: filePath not under projects_dir"
                );
                continue;
            };
            let append_fut = self.inner.store.append(&key, &entries);
            match tokio::time::timeout(self.inner.send_timeout, append_fut).await {
                Ok(Ok(())) => {
                    debug!(
                        file_path,
                        count = entries.len(),
                        "mirrored transcript batch"
                    );
                }
                Ok(Err(err)) => {
                    self.report_error(Some(key), err.to_string());
                }
                Err(_) => {
                    self.report_error(
                        Some(key),
                        format!(
                            "append timed out after {:.1}s",
                            self.inner.send_timeout.as_secs_f64()
                        ),
                    );
                }
            }
        }
    }

    fn report_error(&self, key: Option<SessionKey>, error: String) {
        warn!(?key, %error, "transcript_mirror store.append failed");
        let msg = Message::MirrorError { key, error };
        if self.inner.error_sink.send(msg).is_err() {
            // Receiver hung up — client is tearing down; nothing to do.
        }
    }
}

/// Coalesce buffered entries by `file_path`, preserving first-seen path
/// order and within-path append order.
fn coalesce_by_path(items: Vec<PendingEntry>) -> Vec<(String, Vec<SessionStoreEntry>)> {
    let mut by_path: Vec<(String, Vec<SessionStoreEntry>)> = Vec::new();
    for item in items {
        if let Some(bucket) = by_path.iter_mut().find(|(p, _)| p == &item.file_path) {
            bucket.1.extend(item.entries);
        } else {
            by_path.push((item.file_path, item.entries));
        }
    }
    by_path
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::session_store::{MemorySessionStore, SessionStoreEntry, SessionStoreError};
    use async_trait::async_trait;
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn entry(text: &str) -> SessionStoreEntry {
        SessionStoreEntry {
            ty: "user".into(),
            uuid: None,
            timestamp: None,
            extra: json!({"text": text}),
        }
    }

    fn projects_dir() -> String {
        "/Users/test/.claude/projects".to_string()
    }

    fn session_file_path(project: &str, session: &str) -> String {
        format!("{}/{project}/{session}.jsonl", projects_dir())
    }

    #[tokio::test]
    async fn flush_noop_when_empty() {
        let store = Arc::new(MemorySessionStore::new());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let batcher = TranscriptMirrorBatcher::new(store.clone(), projects_dir(), tx);
        batcher.flush().await;
        assert!(rx.try_recv().is_err(), "no MirrorError expected");
    }

    #[tokio::test]
    async fn enqueue_then_flush_appends_to_store() {
        let store = Arc::new(MemorySessionStore::new());
        let (tx, _rx) = mpsc::unbounded_channel();
        let batcher = TranscriptMirrorBatcher::new(store.clone(), projects_dir(), tx);
        let path = session_file_path("proj", "sess-1");
        batcher.enqueue(path, vec![entry("a"), entry("b")]);
        batcher.flush().await;

        let key = SessionKey {
            project_key: "proj".into(),
            session_id: "sess-1".into(),
            subpath: None,
        };
        let loaded = store.load(&key).await.expect("load").expect("present");
        assert_eq!(loaded.len(), 2);
    }

    #[tokio::test]
    async fn coalesces_multiple_frames_for_same_file() {
        let store = Arc::new(MemorySessionStore::new());
        let (tx, _rx) = mpsc::unbounded_channel();
        let batcher = TranscriptMirrorBatcher::with_thresholds(
            store.clone(),
            projects_dir(),
            tx,
            100,
            1_000_000,
            Duration::from_secs(5),
        );
        let path = session_file_path("proj", "sess-2");
        batcher.enqueue(path.clone(), vec![entry("1"), entry("2")]);
        batcher.enqueue(path.clone(), vec![entry("3")]);
        batcher.enqueue(path, vec![entry("4"), entry("5")]);
        batcher.flush().await;

        let key = SessionKey {
            project_key: "proj".into(),
            session_id: "sess-2".into(),
            subpath: None,
        };
        let loaded = store.load(&key).await.expect("load").expect("present");
        assert_eq!(loaded.len(), 5);
        let texts: Vec<_> = loaded
            .iter()
            .map(|e| e.extra.get("text").and_then(Value::as_str).unwrap())
            .collect();
        assert_eq!(texts, ["1", "2", "3", "4", "5"]);
    }

    #[derive(Default)]
    struct CountingStore {
        count: AtomicUsize,
    }

    #[async_trait]
    impl SessionStore for CountingStore {
        async fn append(
            &self,
            _key: &SessionKey,
            _entries: &[SessionStoreEntry],
        ) -> Result<(), SessionStoreError> {
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn load(
            &self,
            _key: &SessionKey,
        ) -> Result<Option<Vec<SessionStoreEntry>>, SessionStoreError> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn eager_flush_fires_when_entry_threshold_exceeded() {
        let store = Arc::new(CountingStore::default());
        let (tx, _rx) = mpsc::unbounded_channel();
        // 3-entry threshold — second enqueue pushes us to 4 entries, over
        // the limit, triggering an eager drain.
        let batcher = TranscriptMirrorBatcher::with_thresholds(
            store.clone(),
            projects_dir(),
            tx,
            3,
            1_000_000,
            Duration::from_secs(5),
        );
        let path = session_file_path("proj", "sess-3");
        batcher.enqueue(path.clone(), vec![entry("a"), entry("b")]);
        batcher.enqueue(path, vec![entry("c"), entry("d")]);
        batcher.flush().await;
        assert_eq!(store.count.load(Ordering::SeqCst), 1);
    }

    struct FailingStore;

    #[async_trait]
    impl SessionStore for FailingStore {
        async fn append(
            &self,
            _key: &SessionKey,
            _entries: &[SessionStoreEntry],
        ) -> Result<(), SessionStoreError> {
            Err(SessionStoreError::Backend("disk full".to_string()))
        }
        async fn load(
            &self,
            _key: &SessionKey,
        ) -> Result<Option<Vec<SessionStoreEntry>>, SessionStoreError> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn append_failure_emits_mirror_error_on_channel() {
        let store: Arc<dyn SessionStore> = Arc::new(FailingStore);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let batcher = TranscriptMirrorBatcher::new(store, projects_dir(), tx);
        let path = session_file_path("proj", "sess-4");
        batcher.enqueue(path, vec![entry("x")]);
        batcher.flush().await;
        let msg = rx.try_recv().expect("MirrorError emitted");
        match msg {
            Message::MirrorError { key, error } => {
                let key = key.expect("key set");
                assert_eq!(key.project_key, "proj");
                assert_eq!(key.session_id, "sess-4");
                assert!(error.contains("disk full"), "unexpected error: {error}");
            }
            other => panic!("expected MirrorError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn drops_frames_with_filepath_outside_projects_dir() {
        let store = Arc::new(MemorySessionStore::new());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let batcher = TranscriptMirrorBatcher::new(store.clone(), projects_dir(), tx);
        batcher.enqueue(
            "/some/other/path/foo.jsonl".to_string(),
            vec![entry("drop me")],
        );
        batcher.flush().await;
        assert!(
            rx.try_recv().is_err(),
            "filepath-outside-projects should be logged-only"
        );
    }
}
