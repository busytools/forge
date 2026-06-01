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

    /// Construct a `ReplayHarness` around a hand-built [`App`] for
    /// render-snapshot tests that don't need a captured baseline.
    /// The harness still gives `snapshot_chat` / `snapshot_inspector`
    /// access against a `TestBackend`, which is the part replay-driven
    /// tests use to lock layouts into `insta` snapshots.
    pub(crate) fn from_app(app: App) -> Self {
        Self { app }
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

    /// #302: CronCreate's `tool_use_result` envelope carries the
    /// canonical job id at `envelope.id`. A subsequent CronDelete
    /// removes the SCHEDULES entry by that id. Pre-fix, the extractor
    /// stamped the inner content text (the human description) onto
    /// `cron_id`, so the delete never matched and the entry persisted
    /// as a phantom. This test drives the full CronCreate ->
    /// tool_use_result -> CronDelete sequence through the production
    /// reducer (`decode_dispatch` + `apply_session_update`) and
    /// asserts the SCHEDULES bucket drains.
    #[test]
    fn replay_cron_lifecycle_create_then_delete_drains_entry() {
        let session_id = "replay-cron-302";
        let job_id = "d17a030d";
        // 1) Assistant turn: tool_use = CronCreate.
        let create_tool_use = format!(
            "{{\"type\":\"assistant\",\"message\":{{\"model\":\"claude-test\",\
             \"id\":\"msg_cr\",\"type\":\"message\",\"role\":\"assistant\",\
             \"content\":[{{\"type\":\"tool_use\",\"id\":\"tu_create\",\
             \"name\":\"CronCreate\",\"input\":{{\"cron\":\"*/1 * * * *\",\
             \"recurring\":true,\"durable\":false,\"reason\":\"test\"}}}}],\
             \"stop_reason\":null,\"stop_sequence\":null,\"usage\":{{\
             \"input_tokens\":0,\"output_tokens\":0}}}},\
             \"parent_tool_use_id\":null,\"session_id\":\"{session_id}\",\
             \"uuid\":\"uuid-create\"}}"
        );
        // 2) User turn: tool_result envelope. The inner content text
        //    contains the id as a substring (the old extractor's
        //    bug-source); the outer `tool_use_result.id` carries the
        //    canonical id (the new extractor's source).
        let create_result = format!(
            "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\
             \"content\":[{{\"type\":\"tool_result\",\
             \"tool_use_id\":\"tu_create\",\
             \"content\":\"Scheduled recurring job {job_id} (Every minute)\",\
             \"is_error\":false}}]}},\"parent_tool_use_id\":null,\
             \"session_id\":\"{session_id}\",\"uuid\":\"uuid-result\",\
             \"tool_use_result\":{{\"id\":\"{job_id}\",\
             \"humanSchedule\":\"Every minute\",\"recurring\":true,\
             \"durable\":false}}}}"
        );
        // 3) Assistant turn: tool_use = CronDelete, addressing the
        //    job by id.
        let delete_tool_use = format!(
            "{{\"type\":\"assistant\",\"message\":{{\"model\":\"claude-test\",\
             \"id\":\"msg_del\",\"type\":\"message\",\"role\":\"assistant\",\
             \"content\":[{{\"type\":\"tool_use\",\"id\":\"tu_delete\",\
             \"name\":\"CronDelete\",\"input\":{{\"id\":\"{job_id}\"}}}}],\
             \"stop_reason\":null,\"stop_sequence\":null,\"usage\":{{\
             \"input_tokens\":0,\"output_tokens\":0}}}},\
             \"parent_tool_use_id\":null,\"session_id\":\"{session_id}\",\
             \"uuid\":\"uuid-delete\"}}"
        );

        let mut app = App::test_default();
        app.set_session_id(Some(model::SessionId::new(session_id)));
        for (i, line) in [create_tool_use, create_result, delete_tool_use].iter().enumerate() {
            let decoded = decode_dispatch(line, (i + 1) as u64)
                .unwrap_or_else(|err| panic!("decode_dispatch line {}: {err}", i + 1));
            if let DecodedLine::Message(msg) = decoded {
                apply_session_update(
                    &mut app,
                    SessionUpdate::ChatAppended { session_id: session_id.to_owned(), msg },
                );
            }
        }

        let harness = ReplayHarness::from_app(app);
        let schedules = &harness.default_session().schedules;
        assert!(
            schedules.is_empty(),
            "CronDelete must drain the entry; got {} stale entry/entries: {:?}",
            schedules.len(),
            schedules.iter().map(|e| &e.label).collect::<Vec<_>>(),
        );
    }

    /// Chat tool-call grouping (L2 default): five consecutive Read
    /// tool calls in one assistant message collapse to a single
    /// summary line. The baselines on disk don't carry the four
    /// groupable tool names, so the test builds the chat state
    /// in-memory (mirrors the production path's
    /// `push_message_tracked` route) and renders via the existing
    /// `snapshot_chat`.
    #[test]
    fn render_grouped_five_reads_at_l2_summary() {
        let app = build_app_with_consecutive_reads(5);
        let mut harness = ReplayHarness::from_app(app);
        let snapshot = harness.snapshot_chat(80, 20);
        insta::assert_snapshot!(snapshot);
    }

    /// L1: cycling the group lifts the summary into per-tool title
    /// rows (bodies still closed). All five Read rows surface with
    /// their titles.
    #[test]
    fn render_grouped_five_reads_at_l1_titles() {
        let mut app = build_app_with_consecutive_reads(5);
        let leader_id = first_chat_group_leader(&app).expect("baseline produces a group");
        let _ = app.cycle_group_collapse_level(&leader_id);
        let mut harness = ReplayHarness::from_app(app);
        let snapshot = harness.snapshot_chat(80, 20);
        insta::assert_snapshot!(snapshot);
    }

    /// L0: cycling once more renders titles + full bodies via the
    /// standard per-tool path with `force_collapsed = false`.
    #[test]
    fn render_grouped_five_reads_at_l0_bodies() {
        let mut app = build_app_with_consecutive_reads(5);
        let leader_id = first_chat_group_leader(&app).expect("baseline produces a group");
        let _ = app.cycle_group_collapse_level(&leader_id);
        let _ = app.cycle_group_collapse_level(&leader_id);
        let mut harness = ReplayHarness::from_app(app);
        let snapshot = harness.snapshot_chat(80, 40);
        insta::assert_snapshot!(snapshot);
    }

    /// v2: a single Read in an assistant message forms a 1-item
    /// group. At default L2 the render short-circuits to the L1 path
    /// so the actual Read title shows up (no bare "1 read" summary).
    #[test]
    fn render_single_read_renders_title_at_default_l2() {
        let app = build_app_with_consecutive_reads(1);
        let mut harness = ReplayHarness::from_app(app);
        let snapshot = harness.snapshot_chat(80, 10);
        insta::assert_snapshot!(snapshot);
    }

    /// v2: the cycle L2 -> L1 -> L0 -> L2 still walks all three
    /// states for a single-item group. L2 and L1 render identically
    /// (the title row); L0 adds the body. Cycling back to L2 returns
    /// to the title-only view.
    #[test]
    fn render_single_read_cycle_l2_l1_l0_walks_all_states() {
        let mut app = build_app_with_consecutive_reads(1);
        let leader_id = first_chat_group_leader(&app).expect("single-item group present");
        // L2 (default) - identical to L1 for len==1.
        let snap_l2_default = {
            let mut harness = ReplayHarness::from_app(app);
            harness.snapshot_chat(80, 10)
        };
        // Reconstruct because ReplayHarness took ownership.
        let mut app = build_app_with_consecutive_reads(1);
        // L2 -> L1
        let _ = app.cycle_group_collapse_level(&leader_id);
        let snap_l1 = {
            let mut harness = ReplayHarness::from_app(app);
            harness.snapshot_chat(80, 10)
        };
        assert_eq!(
            snap_l2_default, snap_l1,
            "single-item L2 default must render identically to L1",
        );

        // Build a fresh app to reach L0 in one cycle step (cycle is
        // L2 -> L1 -> L0).
        let mut app = build_app_with_consecutive_reads(1);
        let _ = app.cycle_group_collapse_level(&leader_id);
        let _ = app.cycle_group_collapse_level(&leader_id);
        let mut harness = ReplayHarness::from_app(app);
        let snap_l0 = harness.snapshot_chat(80, 20);
        insta::assert_snapshot!("render_single_read_cycle_l0_bodies", snap_l0);
    }

    fn build_app_with_consecutive_reads(n: usize) -> App {
        use crate::agent::model::{self, SessionId};
        use crate::app::{
            BlockCache, ChatMessage, MessageBlock, MessageRole, TerminalSnapshotMode, ToolCallInfo,
        };

        let mut app = App::test_default();
        app.set_session_id(Some(SessionId::new("group-render-test")));
        let blocks: Vec<MessageBlock> = (0..n)
            .map(|i| {
                MessageBlock::ToolCall(Box::new(ToolCallInfo {
                    id: format!("tu-{i}"),
                    title: format!("Read crates/forge-tui/src/{i}.rs"),
                    sdk_tool_name: "Read".to_owned(),
                    raw_input: None,
                    raw_input_bytes: 0,
                    output_metadata: None,
                    task_metadata: None,
                    status: model::ToolCallStatus::Completed,
                    content: Vec::new(),
                    hidden: false,
                    terminal_id: None,
                    terminal_command: None,
                    terminal_output: None,
                    terminal_output_len: 0,
                    terminal_bytes_seen: 0,
                    terminal_snapshot_mode: TerminalSnapshotMode::AppendOnly,
                    monitor_output_tail: Vec::default(),
                    render_epoch: 0,
                    layout_epoch: 0,
                    last_measured_width: 0,
                    last_measured_height: 0,
                    last_measured_layout_epoch: 0,
                    last_measured_layout_generation: 0,
                    cache: BlockCache::default(),
                    collapsed_override: None,
                    last_measured_y_in_msg: 0,
                }))
            })
            .collect();
        app.push_message_tracked(ChatMessage::new(MessageRole::Assistant, blocks, None));
        app
    }

    fn first_chat_group_leader(app: &App) -> Option<crate::ui::message::grouping::GroupId> {
        use crate::ui::message::grouping::{RenderUnit, partition_blocks_into_render_units};
        let session = app.active_session()?;
        for msg in &session.messages {
            for unit in partition_blocks_into_render_units(&msg.blocks) {
                if let RenderUnit::Group { leader_id, .. } = unit {
                    return Some(leader_id);
                }
            }
        }
        None
    }
}
