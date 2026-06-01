//! #280: Replay-driven render-test harness.
//!
//! Eliminates the "passed tests, failed live" bug class that produced the
//! #275 / #276 / #277 cluster (every PR passed hand-built unit tests then
//! shipped a render regression discovered in live use). The harness
//! decodes a real captured `forge-test-harness/baselines/sdk/2.1.156/<name>.jsonl`,
//! drives the App reducer through the production `apply_session_update`
//! path, and exposes both per-session state for assertions and rendered
//! Buffer output for `insta::assert_snapshot!()` checks.
//!
//! ## API
//!
//! - [`replay_baseline(name)`] loads `<name>.jsonl` from the captured
//!   baselines directory, replays every inbound stream-json line
//!   through the App reducer, and returns a [`ReplayHarness`] holding
//!   the final App state.
//! - [`ReplayHarness::default_session`] exposes the active session's
//!   `UiSession` for direct assertions on its fields (`monitors`,
//!   `workflows`, `messages`, etc.). Future helpers covering
//!   multi-session baselines can add a per-key accessor; until then,
//!   one bucket is sufficient.
//! - [`ReplayHarness::snapshot_inspector`] and
//!   [`ReplayHarness::snapshot_chat`] render the active session through
//!   ratatui's `TestBackend` and return a text-only `String` shaped for
//!   `insta::assert_snapshot!`. Style coverage is via explicit
//!   per-span assertions when a particular colour / modifier matters
//!   (text+style snapshots were rejected for review noise per the plan's
//!   "Render snapshot granularity" section).
//!
//! ## Workflow
//!
//! New replay tests opt in by calling `replay_baseline("<name>")` and
//! writing the desired state / render assertions. When a render
//! assertion produces a new snapshot, the test prints a `.snap.new`
//! file alongside the existing one. Run `cargo insta review` to
//! inspect pending diffs; `cargo insta accept` writes the new `.snap`
//! over the old one. Commit the accepted `.snap`.
//!
//! Reviewer guidance: a `.snap` diff means EITHER (a) a real render
//! regression introduced by the PR or (b) a deliberate UX change that
//! the implementer explicitly accepted. The "trust the script, eyeball
//! the snapshot" pattern from #286's bulk scrub applies.
//!
//! ## Scoping
//!
//! - Baselines under `forge-test-harness/baselines/sdk/2.1.156/` were
//!   captured against a LIVE CLI session; any `task_notification.output_file`
//!   path inside them points to a `/private/tmp/claude-*/...` location
//!   that is gone at replay time. The harness replays the wire frames
//!   verbatim; tests that need the watched file's CONTENT live in
//!   `app::monitor_output` (live `write_tmp` unit tests). The harness
//!   covers the wire-driven REDUCER + RENDER paths, which is the bug
//!   class #275 / #276 / #277 produced.
//! - Existing hand-built tests are NOT migrated (Q4 lock from lead's
//!   design picks). New tests opt in.

use std::fs;
use std::path::PathBuf;

use forge_sdk::transport::codec::{DecodedLine, decode_dispatch};
use forge_workspace::SessionUpdate;
use ratatui::Frame;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

use crate::agent::model;
use crate::app::session::UiSession;
use crate::app::{App, apply_session_update};

const BASELINE_DIR_FROM_TUI_CRATE: &str = "../forge-test-harness/baselines/sdk/2.1.156/";

/// Replay-derived state. Holds the App after all baseline lines were
/// driven through the reducer, plus render helpers that snap the
/// Inspector or chat block into a `TestBackend` buffer.
pub(crate) struct ReplayHarness {
    app: App,
}

impl ReplayHarness {
    /// Borrow the active session's [`UiSession`]. Panics if no session
    /// is active - replaying any non-trivial baseline must leave at
    /// least one bucket populated.
    pub(crate) fn default_session(&self) -> &UiSession {
        let key =
            self.app.active_session_key.as_ref().expect("replay must populate active_session_key");
        self.app.sessions.get(key).expect("active_session_key must resolve to a populated bucket")
    }

    /// Mutable handle on the replayed [`App`]. Lets tests drive
    /// production-state mutators (e.g. `replace_monitor_output_tail_by_task_id`)
    /// post-replay to exercise paths the captured baseline doesn't
    /// naturally trigger.
    pub(crate) fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    /// Render the Inspector pane into a `TestBackend` buffer at the
    /// requested dimensions. Returns the rendered text-only multi-line
    /// String suitable for `insta::assert_snapshot!`.
    pub(crate) fn snapshot_inspector(&mut self, width: u16, height: u16) -> String {
        self.snapshot_with(width, height, crate::ui::inspector_pane::render)
    }

    /// Render the chat block into a `TestBackend` buffer at the
    /// requested dimensions. Returns the text-only multi-line String
    /// for `insta::assert_snapshot!`.
    pub(crate) fn snapshot_chat(&mut self, width: u16, height: u16) -> String {
        self.snapshot_with(width, height, crate::ui::chat::render)
    }

    fn snapshot_with(
        &mut self,
        width: u16,
        height: u16,
        render: fn(&mut Frame, Rect, &mut App),
    ) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal =
            ratatui::Terminal::new(backend).expect("TestBackend::new always succeeds");
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, width, height);
                render(frame, area, &mut self.app);
            })
            .expect("TestBackend draw never fails");
        buffer_to_text(terminal.backend().buffer())
    }
}

/// Concatenate the buffer's cells row-by-row into a multi-line string.
/// Trailing whitespace on each row is stripped so review noise stays
/// low; empty trailing rows are kept so the buffer's overall shape
/// (height) is preserved in the snapshot.
fn buffer_to_text(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area;
    let mut out = String::new();
    for y in 0..area.height {
        let mut row = String::new();
        for x in 0..area.width {
            if let Some(cell) = buffer.cell((x, y)) {
                row.push_str(cell.symbol());
            }
        }
        out.push_str(row.trim_end_matches(' '));
        out.push('\n');
    }
    // Drop the very last newline so the snapshot doesn't accumulate a
    // trailing blank when the buffer renders cleanly.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Drive the App reducer over the captured baseline at `<name>.jsonl`.
///
/// Panics on a missing baseline, on a `DecodedLine::Unknown` frame
/// (decoder drift), or on a per-line decode error (wire shape
/// mismatch). Each of these conditions is a hard fail because the
/// baseline + decoder + harness are supposed to round-trip cleanly.
pub(crate) fn replay_baseline(name: &str) -> ReplayHarness {
    let path = baseline_path(name);
    let content = fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "replay_baseline: failed to read baseline at {} - {err}\n\
             (capture via `FORGE_WIRE_CAPTURE=1 cargo nextest run -p forge-test-harness \
             --no-capture --run-ignored only sdk_<scenario>`)",
            path.display()
        )
    });

    let mut app = App::test_default();
    // Adopt a stable session id BEFORE the reducer runs so the
    // session-id guard inside `apply_session_update` accepts each
    // ChatAppended envelope. The pre-Connect bucket migrates onto
    // this key as part of `set_session_id`.
    app.set_session_id(Some(model::SessionId::new("replay-session")));

    for (raw_line_no, raw_line) in content.lines().enumerate() {
        if raw_line.trim().is_empty() || raw_line.starts_with('#') {
            continue;
        }
        // Outer envelope shape: `{"dir":"in|out","line":"<stream-json>"}`.
        // The capture harness wraps every direction-tagged frame so we
        // can replay only the inbound stream (outbound lines are the
        // forge-side control_requests + user messages forge already
        // wrote during capture; the reducer never sees them on the
        // production read path).
        let envelope: CaptureEnvelope = serde_json::from_str(raw_line).unwrap_or_else(|err| {
            panic!("replay_baseline {name}: line {} not a CaptureEnvelope - {err}", raw_line_no + 1)
        });
        if envelope.dir != "in" {
            continue;
        }
        let line_no = (raw_line_no + 1) as u64;
        let decoded = decode_dispatch(&envelope.line, line_no).unwrap_or_else(|err| {
            panic!("replay_baseline {name}: decode_dispatch line {line_no}: {err}")
        });
        match decoded {
            DecodedLine::Message(msg) => {
                apply_session_update(
                    &mut app,
                    SessionUpdate::ChatAppended { session_id: "replay-session".to_owned(), msg },
                );
            }
            // Control + ControlResponse + ControlCancel are part of the
            // forge<->CLI handshake / outbound-cancel surface. Production
            // routes them through the SDK's control loop, NOT the App
            // reducer. Replay skips them - the reducer never sees these
            // frames in live operation.
            DecodedLine::Control(_)
            | DecodedLine::ControlResponse { .. }
            | DecodedLine::ControlCancel { .. } => {}
            DecodedLine::Unknown { type_str, .. } => {
                panic!(
                    "replay_baseline {name}: line {line_no} decoded as Unknown \
                     (type={type_str}). Either the decoder regressed or the baseline \
                     captured a newer wire variant - re-run sdk_wire_conformance to triage."
                );
            }
        }
    }

    ReplayHarness { app }
}

fn baseline_path(name: &str) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push(BASELINE_DIR_FROM_TUI_CRATE);
    path.push(format!("{name}.jsonl"));
    path
}

#[derive(serde::Deserialize)]
struct CaptureEnvelope {
    dir: String,
    line: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_harness_loads_monitor_baseline_without_panic() {
        let harness = replay_baseline("monitor_persistent_stream");
        assert!(
            !harness.app.sessions.is_empty(),
            "replay must populate at least one session bucket"
        );
    }

    /// Wire-driven monitor lifecycle end-state. Backfills the #277 bug
    /// cluster (3a: status transition didn't fire; 5: race between
    /// task_updated terminal and the all-completed-clear).
    ///
    /// Baseline timeline:
    /// - line 19 `assistant` envelope with `tool_use=Monitor` -> the
    ///   reducer creates a `MonitorEntry`, status=Running.
    /// - line 20 `system/task_started` -> `task_id` is stamped onto the
    ///   entry.
    /// - line 22 `system/task_updated` with `patch.status="completed"`
    ///   -> the entry's status flips to `Completed`.
    ///
    /// The baseline does NOT carry a `system/task_notification`. Per the
    /// #277 Bug 5 fix, `set_monitor_status_by_task_id` deferred the
    /// all-completed-clear trigger to `handle_task_notification` so
    /// the `task_updated terminal -> task_notification with output_file`
    /// wire ordering can stamp the tail before the entry drains. So
    /// the correct post-replay end-state for THIS baseline is: 1
    /// entry, status=Completed, NOT drained. A regression of Bug 5
    /// (re-firing the clear inside `set_monitor_status_by_task_id`)
    /// would show 0 entries here.
    ///
    /// `output_tail` CONTENT coverage stays in `app::monitor_output`'s
    /// live-`write_tmp` unit tests (the baseline's captured temp path
    /// is dead at replay time).
    #[test]
    fn replay_monitor_persistent_stream_lands_entry_terminal_not_drained() {
        use crate::app::state::types::MonitorStatus;

        let harness = replay_baseline("monitor_persistent_stream");
        let session = harness.default_session();
        assert_eq!(
            session.monitors.len(),
            1,
            "Bug 5: the all-completed-clear must NOT fire inside \
             set_monitor_status_by_task_id - that trigger is deferred to \
             handle_task_notification. Drift here would re-introduce the \
             pre-#278 race."
        );
        assert_eq!(
            session.monitors[0].status,
            MonitorStatus::Completed,
            "Bug 3a: task_updated terminal must flip the entry's status",
        );
        assert_eq!(
            session.monitors[0].task_id.as_deref(),
            Some("b2q3xiq4o"),
            "task_started must stamp the wire task_id onto the entry",
        );
    }

    /// Monitor's matching `ToolCallInfo` carries the last-5 lines of
    /// the watched command's output once `replace_monitor_output_tail_by_task_id`
    /// fires (the production path the chat live block reads from).
    /// The `monitor_persistent_stream` baseline doesn't include a
    /// `system/task_notification` event with an `output_file`, so the
    /// stamping path isn't naturally exercised - drive it manually
    /// against the replayed Monitor's `task_id` to confirm the
    /// end-to-end indexing + stamping wiring (matches the production
    /// flow that fires when the wire delivers a notification).
    #[test]
    fn replay_monitor_persistent_stream_stamps_chat_tail() {
        use crate::app::MessageBlock;

        let mut harness = replay_baseline("monitor_persistent_stream");
        // Pluck the Monitor's task_id from the replayed session state.
        let task_id = harness
            .default_session()
            .monitors
            .iter()
            .find_map(|m| m.task_id.clone())
            .expect("baseline replays a Monitor with a stamped task_id");

        // Simulate the production task_notification path stamping new
        // output lines onto the matching MonitorEntry + its
        // ToolCallInfo. 8 lines in -> last 5 expected on the chat
        // surface; the unit tests in `state::tests` cover the
        // truncation arithmetic, this asserts the replay-built
        // session state + tool_call_index actually resolve to a
        // mutable ToolCallInfo.
        let lines: Vec<String> = (1..=8).map(|i| format!("line {i}")).collect();
        harness.app_mut().replace_monitor_output_tail_by_task_id(&task_id, &lines);

        let session = harness.default_session();
        let tail: Vec<String> = session
            .messages
            .iter()
            .flat_map(|m| m.blocks.iter())
            .find_map(|block| match block {
                MessageBlock::ToolCall(tc) if tc.sdk_tool_name == "Monitor" => {
                    Some(tc.monitor_output_tail.clone())
                }
                _ => None,
            })
            .expect("baseline's Monitor tool_use produces a matching ToolCall MessageBlock");
        assert_eq!(
            tail,
            vec![
                "line 4".to_owned(),
                "line 5".to_owned(),
                "line 6".to_owned(),
                "line 7".to_owned(),
                "line 8".to_owned(),
            ],
            "ToolCallInfo.monitor_output_tail holds the last-5 lines after the production \
             stamp path runs - the in-chat live block reads from here",
        );
    }

    /// Inspector pane render at the post-replay end-state. The
    /// MONITORS section is gone (Monitor lives in chat now); the
    /// snapshot captures the surviving GIT + post-section chrome.
    /// Drift here would catch the layout-regression class fixed in
    /// #281 / #284 (badge alignment, gutter changes, ragged-column
    /// reintroductions).
    #[test]
    fn replay_monitor_persistent_stream_inspector_render() {
        let mut harness = replay_baseline("monitor_persistent_stream");
        let snapshot = harness.snapshot_inspector(40, 20);
        insta::assert_snapshot!(snapshot);
    }

    /// Chat block render at the post-replay end-state. Catches the
    /// "stop-hook chip" / role-banner / spinner-glyph layout class.
    /// Drift here is a UX-visible chat regression: extra blank line,
    /// missing chip, wrong role colour. Reviewer accepts deliberately
    /// via `cargo insta accept`; otherwise it's a real regression.
    #[test]
    fn replay_monitor_persistent_stream_chat_render() {
        let mut harness = replay_baseline("monitor_persistent_stream");
        let snapshot = harness.snapshot_chat(80, 40);
        insta::assert_snapshot!(snapshot);
    }
}
