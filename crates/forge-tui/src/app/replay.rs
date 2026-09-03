//! #280: Replay-driven render-test harness.
//!
//! Eliminates the "passed tests, failed live" bug class that produced the
//! #275 / #276 / #277 cluster (every PR passed hand-built unit tests then
//! shipped a render regression discovered in live use). The harness
//! decodes a real captured
//! `forge-test-harness/baselines/sdk/<PINNED_CLI_VERSION>/<name>.jsonl`,
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
//! - Baselines under `forge-test-harness/baselines/sdk/<PINNED_CLI_VERSION>/`
//!   were captured against a LIVE CLI session; any `task_notification.output_file`
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

use forge_primitives::Message;
use forge_sdk::transport::codec::{DecodedLine, decode_dispatch};
use forge_workspace::SessionUpdate;
use ratatui::Frame;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

use crate::agent::model;
use crate::app::session::UiSession;
use crate::app::{App, apply_session_update};

/// Replay-derived state. Holds the App after all baseline lines were
/// driven through the reducer, plus render helpers that snap the
/// Inspector or chat block into a `TestBackend` buffer.
pub(crate) struct ReplayHarness {
    app: App,
    result_duration_ms: Option<u64>,
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

    /// Construct a `ReplayHarness` around a hand-built [`App`] for
    /// render-snapshot tests that don't need a captured baseline.
    /// The harness still gives `snapshot_chat` / `snapshot_inspector`
    /// access against a `TestBackend`, which is the part replay-driven
    /// tests use to lock layouts into `insta` snapshots.
    pub(crate) fn from_app(app: App) -> Self {
        Self { app, result_duration_ms: None }
    }

    /// `duration_ms` from the baseline's own `Message::Result` frame, so
    /// a duration assertion tracks the capture instead of a literal every
    /// recapture invalidates. Panics when the baseline has no Result.
    pub(crate) fn result_duration_ms(&self) -> u64 {
        self.result_duration_ms.expect("baseline must carry a Message::Result frame")
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
        render: fn(&mut Frame, Rect, &mut App, &[crate::app::SubagentEntry]),
    ) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal =
            ratatui::Terminal::new(backend).expect("TestBackend::new always succeeds");
        let subagents = self.app.subagents_view();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, width, height);
                render(frame, area, &mut self.app, &subagents);
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
    let mut result_duration_ms = None;
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
                if let Message::Result { duration_ms, .. } = &msg {
                    result_duration_ms = Some(*duration_ms);
                }
                apply_session_update(
                    &mut app,
                    SessionUpdate::ChatAppended { session_id: "replay-session".to_owned(), msg },
                );
            }
            // Control + ControlResponse + ControlCancel are part of the
            // forge<->CLI handshake / outbound-cancel surface. Production
            // routes them through the SDK's control loop, NOT the App
            // reducer. Replay skips them - the reducer never sees these
            // frames in live operation. ToolProgress is dropped by the
            // reader in production, so replay skips it the same way.
            DecodedLine::Control(_)
            | DecodedLine::ControlResponse { .. }
            | DecodedLine::ControlCancel { .. }
            | DecodedLine::ToolProgress(_) => {}
            DecodedLine::Unknown { type_str, .. } => {
                panic!(
                    "replay_baseline {name}: line {line_no} decoded as Unknown \
                     (type={type_str}). Either the decoder regressed or the baseline \
                     captured a newer wire variant - re-run sdk_wire_conformance to triage."
                );
            }
        }
    }

    ReplayHarness { app, result_duration_ms }
}

fn baseline_path(name: &str) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push(format!(
        "../forge-test-harness/baselines/sdk/{}",
        forge_test_harness::PINNED_CLI_VERSION
    ));
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

    /// Wire-driven monitor drain: the 2.1.220 capture carries a
    /// `system/task_notification` for the monitor, so the whole
    /// lifecycle runs end to end through real captured frames.
    ///
    /// Baseline timeline:
    /// - L19 `assistant` `tool_use=Monitor` -> the reducer creates a
    ///   `MonitorEntry` (Running) plus the matching chat `ToolCall`.
    /// - L21 `system/task_started` -> stamps `task_id` onto the entry.
    /// - L24 `system/task_updated` `patch.status="completed"` -> the
    ///   entry flips to `Completed` (the clear is NOT fired here).
    /// - L25 `system/task_notification` -> `handle_task_notification`
    ///   stamps the tail then calls `clear_monitors_if_all_terminal`,
    ///   draining the now-terminal entry from the Inspector.
    ///
    /// Correct end-state: the Monitor `ToolCall` survives in the chat
    /// and the Inspector MONITORS list is empty. The #277 Bug 5
    /// deferral (the status setter must NOT self-clear, so the
    /// notification can stamp the tail before the drain) is locked at
    /// the unit level by the `state::tests` unit tests
    /// (`set_monitor_status_no_longer_clears_implicitly` and
    /// `explicit_clear_drains_when_all_terminal`); this test locks the
    /// wire-driven drain reaching the same end-state.
    #[test]
    fn replay_monitor_persistent_stream_task_notification_drains_completed_monitor() {
        use crate::app::MessageBlock;

        let harness = replay_baseline("monitor_persistent_stream");
        let session = harness.default_session();

        let monitor_created = session
            .messages
            .iter()
            .flat_map(|m| m.blocks.iter())
            .any(|b| matches!(b, MessageBlock::ToolCall(tc) if tc.sdk_tool_name == "Monitor"));
        assert!(
            monitor_created,
            "the L19 Monitor tool_use must produce a chat ToolCall - without it the \
             empty MONITORS assertion below would pass vacuously",
        );
        assert!(
            session.monitors.is_empty(),
            "the L25 task_notification drives handle_task_notification -> \
             clear_monitors_if_all_terminal, draining the completed monitor from the \
             Inspector; got {} entry/entries",
            session.monitors.len(),
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
    /// stop-hook chip / source-label / duration-row / spinner-glyph
    /// layout class. Drift here is a UX-visible chat regression: extra
    /// blank line, missing row, wrong label colour. Reviewer accepts
    /// deliberately via `cargo insta accept`; otherwise it's a real
    /// regression.
    #[test]
    fn replay_monitor_persistent_stream_chat_render() {
        let mut harness = replay_baseline("monitor_persistent_stream");
        let snapshot = harness.snapshot_chat(80, 40);
        insta::assert_snapshot!(snapshot);
    }

    /// `monitor_persistent_stream` fires `thinking_tokens` in its
    /// first turn only and settles four, so the three silent turns
    /// after it are what the accumulator's turn-end reset buys. Remove
    /// that reset - the tempting cleanup once the row keeps its own
    /// copy - and turn one's 83 settles onto all four.
    ///
    /// It does not prove the mirrors overwrite rather than skip: every
    /// turn here opens its own message, so a fresh `None` and a written
    /// `None` are the same thing. That distinction bites on a message a
    /// second turn reuses, which no baseline happens to contain but the
    /// reducer reaches readily - see
    /// `a_turn_reusing_an_unsettled_row_does_not_inherit_its_estimate`.
    #[test]
    fn replay_the_turn_end_reset_keeps_an_estimate_off_later_turns() {
        use crate::app::MessageRole;

        let harness = replay_baseline("monitor_persistent_stream");
        let session = harness.default_session();
        let settled: Vec<Option<u64>> = session
            .messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Assistant) && m.turn_info.is_settled())
            .map(|m| m.turn_info.thinking_tokens)
            .collect();

        assert_eq!(
            settled,
            vec![Some(83), None, None, None],
            "the estimate belongs to the turn that produced it - the three after it fired no \
             event and must not show its number",
        );
    }

    /// `exit_plan_mode` thinks twice in one turn - 50, 164 and then a
    /// restart at 50, 150, 250, 270 - so it is the one baseline where
    /// summing the deltas and reading the wire's counter disagree.
    /// Reading the counter yields the last block's 270 and loses the
    /// first block entirely.
    #[test]
    fn replay_a_multi_block_turn_sums_every_thinking_block() {
        use crate::app::MessageRole;

        let harness = replay_baseline("exit_plan_mode");
        let session = harness.default_session();
        let estimates: Vec<Option<u64>> = session
            .messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Assistant) && m.turn_info.is_settled())
            .map(|m| m.turn_info.thinking_tokens)
            .collect();

        assert_eq!(
            estimates,
            vec![Some(434)],
            "both thinking blocks count toward the turn: 164 and 270 are per-block totals, \
             and the turn spent both",
        );
    }

    /// `compact.jsonl` ends with a Result that has no assistant
    /// message of its own, so it reaches the previous turn's settled
    /// one. Driven through the real reducer because a hand-built
    /// message starts with its token fields unset, which makes
    /// leaving them alone and clearing them indistinguishable.
    #[test]
    fn replay_compact_leaves_a_settled_turn_alone() {
        use crate::app::MessageRole;

        let harness = replay_baseline("compact");
        let compaction_wall = harness.result_duration_ms();
        let session = harness.default_session();
        let settled: Vec<_> = session
            .messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Assistant) && m.turn_info.is_settled())
            .collect();

        assert!(
            !settled.is_empty(),
            "fixture guard: the baseline must settle at least one turn, or the assertions \
             below are vacuous",
        );
        assert!(
            settled.iter().all(|m| m.turn_info.duration_ms != Some(compaction_wall)),
            "the compaction's {compaction_wall}ms clock must not land on a turn that already \
             settled - it would sit over that turn's own token counts and read as one \
             measurement",
        );
        let last = settled.last().expect("checked non-empty above");
        assert!(
            last.turn_info.api_ms.is_some(),
            "and the settled turn keeps its own API time rather than having it erased by the \
             compaction's unattributed zero",
        );
    }

    /// Turn-duration restore: every `Message::Result` carries
    /// `duration_ms` on the wire. The new stamp path in
    /// `handle_result` pulls it out of the destructure and writes
    /// it onto the latest Assistant ChatMessage so the
    /// trailing turn-info row re-renders. This test
    /// drives a real captured baseline through the production reducer
    /// and asserts the stamp lands, comparing against the Result frame
    /// the same replay decoded so a recapture needs no edit here.
    #[test]
    fn replay_permission_suggestions_edit_stamps_turn_duration() {
        use crate::app::MessageRole;

        let harness = replay_baseline("permission_suggestions_edit");
        let expected = harness.result_duration_ms();
        let session = harness.default_session();
        let latest = session
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, MessageRole::Assistant))
            .expect("baseline produces at least one assistant message");
        assert_eq!(
            latest.turn_info.duration_ms,
            Some(expected),
            "Result.duration_ms must stamp onto the latest assistant; got {:?}",
            latest.turn_info.duration_ms,
        );
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

    /// End-to-end: reads carrying an ABSOLUTE `file_path` render
    /// relative to the session cwd through the real chat render path
    /// (the unit tests exercise relative titles + empty cwd, so this is
    /// the only place relativization actually runs against a cwd - a
    /// measure/render desync or a broken cache-fold would surface here).
    #[test]
    fn render_grouped_reads_relativized_against_cwd() {
        let mut app = build_app_with_consecutive_reads(3);
        // Give each read an absolute path under /repo, then point cwd at
        // /repo so relativization has a real prefix to strip.
        if let Some(bucket) = app.try_active_bucket_mut()
            && let Some(msg) = bucket.messages.last_mut()
        {
            for (i, block) in msg.blocks.iter_mut().enumerate() {
                if let crate::app::MessageBlock::ToolCall(tc) = block {
                    tc.raw_input = Some(serde_json::json!({
                        "file_path": format!("/repo/crates/forge-tui/src/{i}.rs")
                    }));
                }
            }
        }
        app.set_cwd_raw("/repo");
        let mut harness = ReplayHarness::from_app(app);
        let snapshot = harness.snapshot_chat(80, 20);
        assert!(
            snapshot.contains("crates/forge-tui/src/0.rs"),
            "read paths relativize against cwd; got:\n{snapshot}",
        );
        assert!(
            !snapshot.contains("/repo/"),
            "the absolute root prefix must be stripped; got:\n{snapshot}",
        );
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

    /// v2 regression-lock: single-item groups behave IDENTICALLY to
    /// multi-item - no `len == 1` special-casing anywhere in the
    /// render dispatch. A lone Read at default L2 renders the tree
    /// summary (parent count row + bare read row + the file nested)
    /// just like a 5-Read group; the cycle walks L2 (summary) -> L1
    /// (title) -> L0 (title + body). Decision 3 lock from the
    /// brainstorm: UI consistency wins over body-visible-by-default;
    /// failed lone tools sit collapsed at L2 and need one ctrl+x to
    /// expand.
    #[test]
    fn render_single_item_group_cycle_walks_l2_l1_l0_like_multi_item() {
        let leader_id = first_chat_group_leader(&build_app_with_consecutive_reads(1))
            .expect("single-item group present");

        let mut harness = ReplayHarness::from_app(build_app_with_consecutive_reads(1));
        let snap_l2 = harness.snapshot_chat(80, 10);
        // A lone read renders the tree parent count row + a bare read row
        // + the sole file nested (project-relative path, lowercase
        // `read`), NOT the full `Read <path>` title row.
        assert!(
            snap_l2.contains("1 tool call"),
            "single-item L2 must carry the tree parent count row; got:\n{snap_l2}",
        );
        assert!(
            snap_l2.contains("\u{2514}\u{2500} crates/forge-tui/src/0.rs"),
            "single-item L2 must nest the file under the read row; got:\n{snap_l2}",
        );
        assert!(
            !snap_l2.contains("Read crates/forge-tui/src/0.rs"),
            "single-item L2 must NOT render the full title row; got:\n{snap_l2}",
        );

        let mut app = build_app_with_consecutive_reads(1);
        let _ = app.cycle_group_collapse_level(&leader_id);
        let snap_l1 = ReplayHarness::from_app(app).snapshot_chat(80, 10);
        assert!(
            snap_l1.contains("Read crates/forge-tui/src/0.rs"),
            "single-item L1 must render the title row; got:\n{snap_l1}",
        );
        assert!(
            !snap_l1.contains("= "),
            "single-item L1 must NOT render the summary group-icon; got:\n{snap_l1}",
        );

        let mut app = build_app_with_consecutive_reads(1);
        let _ = app.cycle_group_collapse_level(&leader_id);
        let _ = app.cycle_group_collapse_level(&leader_id);
        let snap_l0 = ReplayHarness::from_app(app).snapshot_chat(80, 20);
        assert!(
            snap_l0.contains("Read crates/forge-tui/src/0.rs"),
            "single-item L0 must render the title row; got:\n{snap_l0}",
        );
    }

    /// v2.1 (decision 6): a mid-run group renders the L2 summary
    /// with an InProgress aggregate -> the leading status_icon is a
    /// braille spinner frame, not the green check. The snapshot pins
    /// the new line shape; the inline assertion below confirms a
    /// braille codepoint appears in the buffer (the actual spinner
    /// frame varies with the spinner epoch, so we assert the
    /// CLASS of glyph rather than a specific char).
    #[test]
    fn render_in_flight_group_l2_shows_spinner_status_icon() {
        use crate::agent::model::ToolCallStatus;
        let mut app = build_app_with_consecutive_reads(4);
        // Flip the trailing Read to InProgress so the aggregate is
        // InProgress; leave the first three at Completed (the
        // builder default is `Completed`, see below).
        if let Some(session) = app.try_active_bucket_mut()
            && let Some(msg) = session.messages.last_mut()
            && let Some(crate::app::MessageBlock::ToolCall(tc)) = msg.blocks.last_mut()
        {
            tc.status = ToolCallStatus::InProgress;
        }
        let mut harness = ReplayHarness::from_app(app);
        let snap = harness.snapshot_chat(80, 10);
        assert!(
            snap.chars().any(|c| ('\u{2800}'..='\u{28FF}').contains(&c)),
            "in-flight group L2 must show a braille spinner glyph; got:\n{snap}",
        );
        // Pure-read nests: the spinner rides the tree parent count row.
        assert!(
            snap.contains("tool calls"),
            "in-flight group L2 must carry the tree parent count row; got:\n{snap}",
        );
        insta::assert_snapshot!(snap);
    }

    fn build_app_with_consecutive_reads(n: usize) -> App {
        use crate::agent::model::{self, SessionId};
        use crate::app::{BlockCache, ChatMessage, MessageBlock, MessageRole, ToolCallInfo};

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
                    terminal_output: None,
                    monitor_output_tail: Vec::default(),
                    monitor_status: None,
                    render_epoch: 0,
                    layout_epoch: 0,
                    last_measured_width: 0,
                    last_measured_height: 0,
                    last_measured_layout_epoch: 0,
                    last_measured_layout_generation: 0,
                    last_measured_tools_collapsed: false,
                    cache: BlockCache::default(),
                    collapsed_override: None,
                    last_measured_y_in_msg: 0,
                    answered_questions: Vec::new(),
                }))
            })
            .collect();
        app.push_message_tracked(ChatMessage::new(MessageRole::Assistant, blocks));
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
