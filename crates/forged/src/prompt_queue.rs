//! Per-session pending-prompt queue (D14).
//!
//! Holds reverse-RPC prompts that haven't been answered yet — typically
//! because the answering client disconnected mid-request. New clients
//! see the queue contents in their `session.subscribe` response and
//! resolve them via `prompts.respond`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::SystemTime;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

/// What kind of prompt is parked.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum PromptKind {
    /// `permission.request` reverse-RPC.
    Permission,
    /// `hook.<kind>` reverse-RPC. `kind` carries the snake-case hook
    /// name, e.g. `"pre_tool_use"`.
    Hook {
        /// Hook kind in snake-case form (e.g. `"pre_tool_use"`).
        kind: String,
    },
}

impl PromptKind {
    /// Wire-shape kind string for §7.4.12 `pending_prompts[*].kind`.
    #[must_use]
    pub fn as_wire(&self) -> String {
        match self {
            Self::Permission => "permission.request".into(),
            Self::Hook { kind } => format!("hook.{kind}"),
        }
    }
}

/// One entry in the queue. The `responder` oneshot is consumed when the
/// prompt is taken; subsequent attempts to find the same `prompt_id`
/// return `None`.
pub struct PendingPrompt {
    /// Prompt identifier (`prompt_<uuid>`) the client uses to respond.
    pub prompt_id: String,
    /// What kind of prompt this is (permission vs which hook).
    pub kind: PromptKind,
    /// Wall-clock instant the prompt was issued.
    pub issued_at: SystemTime,
    /// Wall-clock instant the prompt expires.
    pub expires_at: SystemTime,
    /// Original reverse-RPC params — surfaced to a new client when they
    /// reconnect and read the queue.
    pub params: serde_json::Value,
    /// Resumes the SDK-side handler waiting for this answer. The queue
    /// takes ownership; on `take()`, ownership transfers to the caller,
    /// who sends the response and lets the oneshot drop.
    pub responder: oneshot::Sender<serde_json::Value>,
}

impl std::fmt::Debug for PendingPrompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingPrompt")
            .field("prompt_id", &self.prompt_id)
            .field("kind", &self.kind)
            .field("issued_at", &self.issued_at)
            .field("expires_at", &self.expires_at)
            .field("params", &self.params)
            .field("responder", &"<oneshot>")
            .finish()
    }
}

/// Wire-shape view of a pending prompt (no `responder` field) — surfaced
/// to subscribers as part of `session.subscribe` responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PendingPromptView {
    /// Prompt identifier.
    pub prompt_id: String,
    /// `permission.request` or `hook.<kind>`.
    pub kind: String,
    /// ISO-8601 timestamp the prompt was issued.
    pub issued_at: String,
    /// ISO-8601 timestamp the prompt expires.
    pub expires_at: String,
    /// Original reverse-RPC params.
    pub params: serde_json::Value,
}

/// FIFO queue of pending prompts. Cheap to clone — internally `Arc`-backed.
#[derive(Debug, Default, Clone)]
pub struct PromptQueue {
    inner: Arc<Mutex<VecDeque<PendingPrompt>>>,
}

impl PromptQueue {
    /// Construct an empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a new prompt to the back of the queue.
    pub fn enqueue(&self, prompt: PendingPrompt) {
        self.inner.lock().push_back(prompt);
    }

    /// Remove the prompt with `prompt_id`, returning ownership to the caller.
    /// `None` if the id is unknown (already taken or never enqueued).
    #[must_use]
    pub fn take(&self, prompt_id: &str) -> Option<PendingPrompt> {
        let mut q = self.inner.lock();
        let pos = q.iter().position(|p| p.prompt_id == prompt_id)?;
        q.remove(pos)
    }

    /// Snapshot of all currently-pending prompt ids — for diagnostics.
    #[must_use]
    pub fn snapshot(&self) -> Vec<String> {
        self.inner
            .lock()
            .iter()
            .map(|p| p.prompt_id.clone())
            .collect()
    }

    /// Wire-shape view for inclusion in `session.subscribe` responses.
    /// Strips the `responder` field; emits ISO-8601 timestamps.
    #[must_use]
    pub fn snapshot_for_wire(&self) -> Vec<PendingPromptView> {
        self.inner
            .lock()
            .iter()
            .map(|p| PendingPromptView {
                prompt_id: p.prompt_id.clone(),
                kind: p.kind.as_wire(),
                issued_at: format_iso8601(p.issued_at),
                expires_at: format_iso8601(p.expires_at),
                params: p.params.clone(),
            })
            .collect()
    }
}

/// Hand-rolled ISO-8601 conversion to keep the dep set minimal.
/// Good enough for v1 wire-shape display; swap to `time` or `chrono`
/// if precision / leap-seconds matter downstream.
fn format_iso8601(t: SystemTime) -> String {
    let dur = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs();
    let (y, m, d, hh, mm, ss) = secs_to_ymdhms(secs);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

#[allow(clippy::cast_possible_truncation)]
fn secs_to_ymdhms(mut secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let ss = (secs % 60) as u32;
    secs /= 60;
    let mm = (secs % 60) as u32;
    secs /= 60;
    let hh = (secs % 24) as u32;
    secs /= 24;
    let mut days = secs as u32;
    let mut y: u32 = 1970;
    loop {
        let in_year = days_in_year(y);
        if days < in_year {
            break;
        }
        days -= in_year;
        y += 1;
    }
    let mut m: u32 = 1;
    loop {
        let in_month = days_in_month(y, m);
        if days < in_month {
            break;
        }
        days -= in_month;
        m += 1;
    }
    (y, m, days + 1, hh, mm, ss)
}

const fn is_leap(y: u32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
}

const fn days_in_year(y: u32) -> u32 {
    if is_leap(y) { 366 } else { 365 }
}

const fn days_in_month(y: u32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}
