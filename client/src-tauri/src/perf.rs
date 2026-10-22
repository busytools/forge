//! The client's perf log: the frame records the page measures, written
//! beside the shell's own log.
//!
//! The page owns the measurement (`client/src/perf.ts`); this is only the
//! write, because a webview has no file to append to. Records arrive as one
//! typed batch over the invoke surface and leave as one JSON line each,
//! under a size cap that rolls to a single `.1` - the log is read after the
//! fact, and the newest window is what a reader wants.
//!
//! A write that fails is answered, never thrown: the page's sink swallows it
//! by design, and this side logs the reason so the failure is not silent.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use tauri::Manager as _;

/// Where the perf log sits beside the shell's own `forge.log`.
const PERF_LOG_NAME: &str = "forge-perf.log";

/// The size one file may reach before it rolls to `.1`.
const MAX_BYTES: u64 = 10 * 1024 * 1024;

/// One record as the page serialises it (`client/src/perf.ts`).
///
/// Every field past the identity is optional because the kinds differ: a
/// frame carries `ms` and its counter, a long task a `name`, a summary the
/// window's counts. Unknown fields are tolerated, so a page that learns a
/// field reaches a shell that predates it rather than being refused.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct ClientPerfRecord {
    pub schema: String,
    pub kind: String,
    pub run_id: String,
    pub ts_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<f64>,
    /// Which terminal state completed a seat's open, on a `drawn` record.
    ///
    /// A plain string rather than an enum: a page that learns a fifth state
    /// must reach this shell, and a strict set would refuse the whole batch
    /// over one unknown word.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// The Web Worker heartbeat spanning a slow frame (#2374): how many
    /// ticks arrived, the WORST lateness among them, when the last one
    /// landed, and the tick interval those are read against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_beats: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_max_late_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_since_beat_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_expected_beats: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_tick_ms: Option<u64>,
    /// Whether a Web Worker heartbeat is running: on `run_started`, that the
    /// page has one at all; a `heartbeat` record with false says it failed
    /// to load and the beat fields are absent from then on (#2374).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slow: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ms: Option<f64>,
    /// #2513's own marks: the click/subscribe/snapshot records and the
    /// connection window's counters. A field this struct does not know is
    /// parsed away before the write, so each one must be named here to reach
    /// the log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declare: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_click_ms: Option<f64>,
    /// The desktop shell's own queue-and-write time for a `subscribe` (#2513).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_waited_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chars: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parse_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscribes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_chars: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscribe_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_chars_per_s: Option<f64>,
}

/// One batch of the page's records into the perf log.
///
/// The page sends records as it finishes them (a slow frame, a summary, a
/// long task), so a batch is usually one line; the batch shape is what
/// keeps the boundary one call rather than one per kind.
///
/// **`async`, though the body is sync**: a sync command runs on the main
/// thread, and these appends are file IO on the path the log measures.
#[tauri::command(async)]
pub fn client_perf(app: tauri::AppHandle, records: Vec<ClientPerfRecord>) -> Result<(), String> {
    let dir = app.path().app_log_dir().map_err(|err| err.to_string())?;
    append(&dir, &records, MAX_BYTES).map_err(|err| {
        tauri_plugin_log::log::warn!("perf write failed: {err}");
        err.to_string()
    })
}

/// The write itself, over a directory a test can hand it.
fn append(dir: &Path, records: &[ClientPerfRecord], max_bytes: u64) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(PERF_LOG_NAME);
    let current = std::fs::metadata(&path).map_or(0, |meta| meta.len());
    if current >= max_bytes {
        let rolled = rolled_path(dir);
        let _ = std::fs::remove_file(&rolled);
        std::fs::rename(&path, &rolled)?;
    }
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    for record in records {
        let line = serde_json::to_string(record).map_err(std::io::Error::other)?;
        writeln!(file, "{line}")?;
    }
    Ok(())
}

fn rolled_path(dir: &Path) -> PathBuf {
    dir.join(format!("{PERF_LOG_NAME}.1"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(kind: &str) -> ClientPerfRecord {
        ClientPerfRecord {
            schema: "forge-perf/v1".to_owned(),
            kind: kind.to_owned(),
            run_id: "run-1".to_owned(),
            ts_ms: 12,
            version: None,
            frame: Some(3),
            ms: Some(120.0),
            state: None,
            worker_beats: None,
            worker_max_late_ms: None,
            worker_since_beat_ms: None,
            worker_expected_beats: None,
            worker_tick_ms: None,
            heartbeat: None,
            name: None,
            surface: Some("session".to_owned()),
            session: Some("forge/lead".to_owned()),
            window_ms: None,
            frames: None,
            slow: None,
            max_ms: None,
            auto: None,
            declare: None,
            since_click_ms: None,
            write_waited_ms: None,
            chars: None,
            parse_ms: None,
            subscribes: None,
            snapshot_chars: None,
            subscribe_rate: None,
            snapshot_chars_per_s: None,
        }
    }

    #[test]
    fn the_batch_lands_as_one_json_line_each() {
        let dir = tempfile::tempdir().expect("tempdir");
        append(dir.path(), &[record("run_started"), record("frame")], MAX_BYTES).expect("append");

        let text = std::fs::read_to_string(dir.path().join(PERF_LOG_NAME)).expect("read back");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one line per record: {text}");
        let parsed: serde_json::Value = serde_json::from_str(lines[1]).expect("a JSON line");
        assert_eq!(parsed["kind"], "frame");
        assert_eq!(parsed["ms"], 120.0);
        assert_eq!(parsed["session"], "forge/lead");
        assert_eq!(parsed["schema"], "forge-perf/v1");
    }

    /// A page that learns a field must reach this shell, not be refused by
    /// it: the boundary narrows once, and this is not that narrowing.
    #[test]
    fn an_unknown_field_from_a_newer_page_is_tolerated() {
        let line =
            r#"{"schema":"forge-perf/v1","kind":"frame","run_id":"r","ts_ms":1,"invented":true}"#;
        let parsed: ClientPerfRecord =
            serde_json::from_str(line).expect("a new field is tolerated");
        assert_eq!(parsed.kind, "frame");
        assert!(parsed.ms.is_none(), "an absent optional is absence, not zero");
    }

    /// **The state a drawn mark carries survives the write.** The page marks
    /// which terminal state completed a seat's open, and this struct is what
    /// re-serialises it: a field the struct does not know is parsed away and
    /// never reaches the log, so the round trip is the property that matters,
    /// not the parse.
    #[test]
    fn the_state_a_drawn_mark_carries_survives_the_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut mark = record("drawn");
        mark.state = Some("waking".to_owned());
        append(dir.path(), &[mark], MAX_BYTES).expect("append");

        let text = std::fs::read_to_string(dir.path().join(PERF_LOG_NAME)).expect("read back");
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).expect("a JSON line");
        assert_eq!(
            parsed["state"], "waking",
            "the shell must write the state the page marked: {text}",
        );
    }

    /// A state the page has not been taught yet is carried rather than
    /// refused: a future fifth state must not cost the whole batch.
    #[test]
    fn an_unknown_state_value_is_carried_rather_than_refused() {
        let line = r#"{"schema":"forge-perf/v1","kind":"drawn","run_id":"r","ts_ms":1,"state":"invented"}"#;
        let parsed: ClientPerfRecord =
            serde_json::from_str(line).expect("an unknown state value is carried");
        assert_eq!(parsed.state.as_deref(), Some("invented"));
    }

    /// **The heartbeat evidence on a slow frame survives the write.** The
    /// worker's beats are the only split between a busy main thread and a
    /// page that stopped being scheduled, so the fields must reach the log
    /// exactly as the page wrote them.
    #[test]
    fn the_heartbeat_evidence_on_a_slow_frame_survives_the_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut slow = record("frame");
        slow.worker_beats = Some(2);
        slow.worker_max_late_ms = Some(1200.5);
        slow.worker_since_beat_ms = Some(80.0);
        slow.worker_expected_beats = Some(5);
        slow.worker_tick_ms = Some(1_000);
        append(dir.path(), &[slow], MAX_BYTES).expect("append");

        let text = std::fs::read_to_string(dir.path().join(PERF_LOG_NAME)).expect("read back");
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).expect("a JSON line");
        assert_eq!(parsed["worker_beats"], 2, "the beat count survives: {text}");
        assert_eq!(parsed["worker_max_late_ms"], 1200.5, "and the worst lateness: {text}");
        assert_eq!(parsed["worker_since_beat_ms"], 80.0, "and the last arrival: {text}");
        assert_eq!(parsed["worker_expected_beats"], 5, "and the beats the gap was due: {text}");
        assert_eq!(parsed["worker_tick_ms"], 1_000, "and the tick it is read against: {text}");
    }

    /// A heartbeat record saying the worker failed to load survives too, so
    /// a broken asset is visible in the log rather than reading as a quiet
    /// page.
    #[test]
    fn a_heartbeat_record_saying_the_worker_failed_survives_the_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut failed = record("heartbeat");
        failed.heartbeat = Some(false);
        failed.name = Some("worker_load_failed: SecurityError: blocked".to_owned());
        append(dir.path(), &[failed], MAX_BYTES).expect("append");

        let text = std::fs::read_to_string(dir.path().join(PERF_LOG_NAME)).expect("read back");
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).expect("a JSON line");
        assert_eq!(parsed["heartbeat"], false, "the failure flag survives: {text}");
        assert_eq!(
            parsed["name"], "worker_load_failed: SecurityError: blocked",
            "and the cause it carries: {text}",
        );
    }

    /// The other half: a record with none of the heartbeat fields writes no
    /// key at all, so a frame on a page without a worker cannot read as a
    /// zero-beat one. A dropped `skip_serializing_if` would write nulls.
    #[test]
    fn a_record_without_heartbeat_evidence_writes_no_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        append(dir.path(), &[record("frame")], MAX_BYTES).expect("append");

        let text = std::fs::read_to_string(dir.path().join(PERF_LOG_NAME)).expect("read back");
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).expect("a JSON line");
        for key in [
            "worker_beats",
            "worker_max_late_ms",
            "worker_since_beat_ms",
            "worker_expected_beats",
            "worker_tick_ms",
            "heartbeat",
        ] {
            assert!(parsed.get(key).is_none(), "no {key} on a page without one: {text}");
        }
    }

    /// **The switch gap's marks survive the write (#2513).** The click, the
    /// subscribe with its unprompted flag, the snapshot's size and parse, and
    /// the connection window's counters are the evidence this instrument
    /// exists for; a field the shell does not name is parsed away silently.
    #[test]
    fn the_switch_gap_marks_survive_the_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut click = record("click");
        click.session = Some("forge/lead".to_owned());
        let mut subscribe = record("subscribe");
        subscribe.auto = Some(true);
        subscribe.declare = Some(false);
        subscribe.since_click_ms = Some(1_750.5);
        subscribe.write_waited_ms = Some(12.5);
        let mut snapshot = record("snapshot");
        snapshot.chars = Some(2_200_000);
        snapshot.parse_ms = Some(38.5);
        let mut connection = record("connection");
        connection.subscribes = Some(15);
        connection.snapshot_chars = Some(33_000_000);
        connection.subscribe_rate = Some(1.5);
        connection.snapshot_chars_per_s = Some(3_300_000.0);
        append(dir.path(), &[click, subscribe, snapshot, connection], MAX_BYTES).expect("append");

        let text = std::fs::read_to_string(dir.path().join(PERF_LOG_NAME)).expect("read back");
        let lines: Vec<serde_json::Value> =
            text.lines().map(|line| serde_json::from_str(line).expect("a JSON line")).collect();
        assert_eq!(lines[1]["auto"], true, "the unprompted flag survives: {text}");
        assert_eq!(lines[1]["declare"], false, "and the declare flag: {text}");
        assert_eq!(lines[1]["since_click_ms"], 1_750.5, "and the click gap: {text}");
        assert_eq!(lines[1]["write_waited_ms"], 12.5, "and the shell's write wait: {text}");
        assert_eq!(lines[2]["chars"], 2_200_000, "the snapshot's length: {text}");
        assert_eq!(lines[2]["parse_ms"], 38.5, "and its parse time: {text}");
        assert_eq!(lines[3]["subscribes"], 15, "the window's subscribe count: {text}");
        assert_eq!(lines[3]["snapshot_chars"], 33_000_000, "and its snapshot chars: {text}");
        assert_eq!(lines[3]["subscribe_rate"], 1.5, "and the rate: {text}");
        assert_eq!(lines[3]["snapshot_chars_per_s"], 3_300_000.0, "and the char rate: {text}");
    }

    /// **The page's own JSON is the boundary (#2513).** A field the struct
    /// does not name is parsed away before the write, so this pins the page's
    /// shapes rather than a hand-built struct: a `subscribe` with its flags
    /// and click gap, and a `snapshot` with its size and parse time.
    #[test]
    fn a_page_record_with_the_switch_gap_marks_reaches_the_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let page = concat!(
            r#"{"schema":"forge-perf/v1","kind":"subscribe","run_id":"r","ts_ms":1,"#,
            r#""session":"forge/lead","declare":false,"auto":true,"since_click_ms":1750.5,"#,
            r#""write_waited_ms":12.5}"#,
            "\n",
            r#"{"schema":"forge-perf/v1","kind":"snapshot","run_id":"r","ts_ms":2,"#,
            r#""chars":2200000,"parse_ms":38.5,"surface":"home"}"#,
        );
        let records: Vec<ClientPerfRecord> = page
            .lines()
            .map(|line| serde_json::from_str(line).expect("the page's line parses"))
            .collect();
        append(dir.path(), &records, MAX_BYTES).expect("append");

        let text = std::fs::read_to_string(dir.path().join(PERF_LOG_NAME)).expect("read back");
        assert!(text.contains("\"auto\":true"), "the flag reaches the log: {text}");
        assert!(text.contains("\"since_click_ms\":1750.5"), "and the click gap: {text}");
        assert!(text.contains("\"chars\":2200000"), "the snapshot's size reaches it: {text}");
        assert!(text.contains("\"parse_ms\":38.5"), "and its parse time: {text}");
    }

    /// The other half: a record with no state writes no key at all, so a
    /// frame cannot read as a drawn mark.
    #[test]
    fn a_record_without_a_state_writes_no_state_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        append(dir.path(), &[record("frame")], MAX_BYTES).expect("append");

        let text = std::fs::read_to_string(dir.path().join(PERF_LOG_NAME)).expect("read back");
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).expect("a JSON line");
        assert!(parsed.get("state").is_none(), "no state on a frame: {text}");
    }

    /// The cap is armed at its own boundary: a file exactly AT the cap
    /// rolls to `.1` and the fresh file holds only what came after.
    #[test]
    fn a_file_at_the_cap_rolls_to_one_before_the_next_batch() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(PERF_LOG_NAME), "x".repeat(50)).expect("seed at the cap");
        append(dir.path(), &[record("summary")], 50).expect("append");

        assert!(rolled_path(dir.path()).exists(), "a file exactly at the cap rolls to .1");
        let fresh = std::fs::read_to_string(dir.path().join(PERF_LOG_NAME)).expect("read back");
        assert_eq!(fresh.lines().count(), 1, "only the new batch is in the fresh file: {fresh}");
    }
}
