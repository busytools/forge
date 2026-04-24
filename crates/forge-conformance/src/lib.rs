//! Utilities for the wire-conformance harness.
//!
//! Intentionally separate from `forge-sdk` so the SDK crate surface
//! stays focused on what library consumers use. Nothing here is public
//! SDK API.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use forge_sdk::Error;
use forge_sdk::transport::Transport;
use forge_sdk::transport::process::Subprocess;

/// One captured line in a trace.
#[derive(Default, Debug)]
pub struct TraceLog {
    /// `(direction, line)` pairs in the order they happened on the wire.
    /// Direction is `"in"` (CLI → SDK, from stdout) or `"out"` (SDK → CLI, to stdin).
    entries: Vec<(&'static str, String)>,
}

impl TraceLog {
    /// Serialise as JSONL: one `{"dir":"in"|"out","line":"..."}` per line.
    ///
    /// # Errors
    ///
    /// Returns a `serde_json::Error` if any entry fails to serialise.
    pub fn to_jsonl(&self) -> Result<String, serde_json::Error> {
        let mut body = String::new();
        for (dir, line) in &self.entries {
            let obj = serde_json::json!({ "dir": dir, "line": line });
            body.push_str(&serde_json::to_string(&obj)?);
            body.push('\n');
        }
        Ok(body)
    }

    /// Slice of inbound lines (CLI → SDK).
    #[must_use]
    pub fn inbound(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|(d, _)| *d == "in")
            .map(|(_, l)| l.as_str())
            .collect()
    }

    /// Slice of outbound lines (SDK → CLI).
    #[must_use]
    pub fn outbound(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|(d, _)| *d == "out")
            .map(|(_, l)| l.as_str())
            .collect()
    }
}

/// Transport wrapper that tees every line through to a shared log while
/// delegating the actual I/O to the wrapped `Subprocess`.
pub struct RecordingTransport {
    inner: Subprocess,
    log: Arc<Mutex<TraceLog>>,
}

impl RecordingTransport {
    /// Wrap a live `Subprocess`. Returns the wrapper + a shared handle to
    /// the trace log so the caller can read it back after shutdown.
    #[must_use]
    pub fn new(inner: Subprocess) -> (Self, Arc<Mutex<TraceLog>>) {
        let log = Arc::new(Mutex::new(TraceLog::default()));
        (
            Self {
                inner,
                log: log.clone(),
            },
            log,
        )
    }
}

#[async_trait]
impl Transport for RecordingTransport {
    async fn read_line(&mut self) -> Result<Option<String>, Error> {
        let line = self.inner.read_line().await?;
        if let Some(ref s) = line {
            self.log.lock().unwrap().entries.push(("in", s.clone()));
        }
        Ok(line)
    }

    async fn write_line(&mut self, line: &str) -> Result<(), Error> {
        self.log
            .lock()
            .unwrap()
            .entries
            .push(("out", line.trim_end_matches('\n').to_string()));
        self.inner.write_line(line).await
    }

    async fn end_input(&mut self) -> Result<(), Error> {
        self.inner.end_input().await
    }

    async fn close(&mut self) -> Result<(), Error> {
        self.inner.close().await
    }
}
