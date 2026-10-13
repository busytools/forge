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
}

/// One batch of the page's records into the perf log.
///
/// The page sends records as it finishes them (a slow frame, a summary, a
/// long task), so a batch is usually one line; the batch shape is what
/// keeps the boundary one call rather than one per kind.
#[tauri::command]
pub fn client_perf(
    app: tauri::AppHandle,
    records: Vec<ClientPerfRecord>,
) -> Result<(), String> {
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
            name: None,
            surface: Some("session".to_owned()),
            session: Some("forge/lead".to_owned()),
            window_ms: None,
            frames: None,
            slow: None,
            max_ms: None,
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
        let line = r#"{"schema":"forge-perf/v1","kind":"frame","run_id":"r","ts_ms":1,"invented":true}"#;
        let parsed: ClientPerfRecord = serde_json::from_str(line).expect("a new field is tolerated");
        assert_eq!(parsed.kind, "frame");
        assert!(parsed.ms.is_none(), "an absent optional is absence, not zero");
    }

    /// The cap is armed: a file that reached it rolls to `.1` and the fresh
    /// file holds only what came after.
    #[test]
    fn a_file_at_the_cap_rolls_to_one_before_the_next_batch() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(PERF_LOG_NAME), "x".repeat(100)).expect("seed");
        append(dir.path(), &[record("summary")], 50).expect("append");

        assert!(rolled_path(dir.path()).exists(), "the over-cap file rolled to .1");
        let fresh = std::fs::read_to_string(dir.path().join(PERF_LOG_NAME)).expect("read back");
        assert_eq!(fresh.lines().count(), 1, "only the new batch is in the fresh file: {fresh}");
    }
}
