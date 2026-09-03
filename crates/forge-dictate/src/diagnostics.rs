//! Per-take diagnostics written to disk, best-effort.
//!
//! One take becomes one directory holding the original capture, every
//! transcription stage, and the timings - so "which stage ate the
//! words" is answerable by opening files rather than by rerunning the
//! take under investigation. The layout is granular on purpose: the
//! pre-normalization text is kept per window, because the joined form
//! cannot show whether a window lost the words or the join did.
//!
//! Everything here is best-effort: a capture that cannot be written is
//! logged and dropped, never propagated, because diagnostics must
//! never break a take.
//!
//! Retention: the store keeps the last [`RETAINED_TAKES`] takes,
//! pruning the oldest by directory name after each write.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

use crate::audio::SAMPLE_RATE;
use crate::engine::Stages;

/// One window's record: the slice of capture it covered and the raw
/// pre-normalization text it produced.
pub(crate) struct WindowRecord {
    pub(crate) start_ms: u64,
    pub(crate) end_ms: u64,
    pub(crate) raw: String,
}

/// Everything one finished take contributes to the store.
pub(crate) struct TakeRecord<'a> {
    pub(crate) audio: &'a [f32],
    pub(crate) windows: &'a [WindowRecord],
    /// The exact normalizer input.
    pub(crate) joined: &'a str,
    pub(crate) text: &'a str,
    pub(crate) stages: &'a Stages,
    pub(crate) processing_ms: u64,
    pub(crate) truncated: bool,
    /// `transcript`, `empty` or `recognition_error`.
    pub(crate) outcome: &'a str,
}

/// How many takes the store keeps. Ten typical dictations are a few
/// hundred megabytes; anything older than the tenth take is gone.
const RETAINED_TAKES: usize = 10;

/// The unix-millisecond stamp a take directory is named by, as
/// `take-<13 digits>`: sortable, and 13 digits holds until the year
/// 2286 so lexicographic order never lies about recency.
pub(crate) fn take_stamp() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis())
}

/// Write one take into `dir/take-<take_id>/` and prune the store to
/// [`RETAINED_TAKES`]. Nothing here can fail the caller: every step
/// logs its own failure and stops that take's capture.
pub(crate) fn capture_take(dir: &Path, take_id: u128, take: &TakeRecord<'_>) {
    let take_dir = dir.join(format!("take-{take_id:013}"));
    if let Err(error) = std::fs::create_dir_all(take_dir.join("raw")) {
        tracing::warn!(%error, dir = %take_dir.display(), "diagnostics: store directory not writable");
        return;
    }

    if let Err(error) = write_wav(&take_dir.join("output.wav"), take.audio) {
        tracing::warn!(%error, dir = %take_dir.display(), "diagnostics: capture not written");
        return;
    }
    for (k, window) in take.windows.iter().enumerate() {
        if let Err(error) = std::fs::write(take_dir.join(format!("raw/{k}.txt")), &window.raw) {
            tracing::warn!(%error, dir = %take_dir.display(), window = k, "diagnostics: window transcript not written");
            return;
        }
    }
    if let Err(error) = std::fs::write(take_dir.join("joined.txt"), take.joined) {
        tracing::warn!(%error, dir = %take_dir.display(), "diagnostics: joined transcript not written");
        return;
    }
    if let Err(error) = std::fs::write(take_dir.join("text.txt"), take.text) {
        tracing::warn!(%error, dir = %take_dir.display(), "diagnostics: normalized transcript not written");
        return;
    }

    let ms = |d: std::time::Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    let meta = json!({
        "duration_ms": ms(take.stages.audio),
        "processing_ms": take.processing_ms,
        "truncated": take.truncated,
        "outcome": take.outcome,
        "stages_ms": {
            "mel": ms(take.stages.mel),
            "encode": ms(take.stages.encode),
            "decode": ms(take.stages.decode),
        },
        "windows": take.windows.iter().enumerate().map(|(k, window)| json!({
            "index": k,
            "file": format!("raw/{k}.txt"),
            "start_ms": window.start_ms,
            "end_ms": window.end_ms,
        })).collect::<Vec<_>>(),
    });
    match serde_json::to_vec_pretty(&meta) {
        Ok(bytes) => {
            if let Err(error) = std::fs::write(take_dir.join("meta.json"), bytes) {
                tracing::warn!(%error, dir = %take_dir.display(), "diagnostics: metadata not written");
                return;
            }
        }
        Err(error) => {
            tracing::warn!(%error, dir = %take_dir.display(), "diagnostics: metadata not serializable");
            return;
        }
    }

    prune(dir);
}

/// Encode `audio` as a canonical 16-bit PCM wav at the one rate every
/// model here reads.
fn write_wav(path: &Path, audio: &[f32]) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).map_err(|error| error.to_string())?;
    for &sample in audio {
        let quantized = (sample * 32767.0).clamp(-32768.0, 32767.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let value = quantized.round() as i16;
        writer.write_sample(value).map_err(|error| error.to_string())?;
    }
    writer.finalize().map_err(|error| error.to_string())
}

/// Delete every take but the newest [`RETAINED_TAKES`], best-effort:
/// a directory that will not leave is left behind rather than fought.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut takes: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.file_name().is_some_and(starts_take))
        .collect();
    takes.sort();
    for stale in takes.iter().rev().skip(RETAINED_TAKES) {
        let _ = std::fs::remove_dir_all(stale);
    }
}

fn starts_take(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|s| s.starts_with("take-"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn take_record<'a>(
        audio: &'a [f32],
        windows: &'a [WindowRecord],
        joined: &'a str,
        text: &'a str,
        stages: &'a Stages,
    ) -> TakeRecord<'a> {
        TakeRecord {
            audio,
            windows,
            joined,
            text,
            stages,
            processing_ms: 7,
            truncated: false,
            outcome: "transcript",
        }
    }

    /// The store mirrors the granular layout: capture, per-window raw
    /// transcripts, the exact normalizer input, the final text, and the
    /// metadata that ties them together.
    #[test]
    fn a_take_writes_the_whole_store() {
        let dir = tempfile::tempdir().unwrap();
        let audio = vec![0.5; SAMPLE_RATE as usize];
        let stages = Stages { audio: Duration::from_millis(1000), ..Stages::default() };
        let windows = vec![
            WindowRecord { start_ms: 0, end_ms: 500, raw: "first take".into() },
            WindowRecord { start_ms: 500, end_ms: 1000, raw: "second take".into() },
        ];
        capture_take(
            dir.path(),
            42,
            &take_record(
                &audio,
                &windows,
                "first take second take",
                "First take, second take.",
                &stages,
            ),
        );

        let take = dir.path().join("take-0000000000042");
        let mut reader = hound::WavReader::open(take.join("output.wav")).unwrap();
        assert_eq!(
            reader.spec().sample_rate,
            SAMPLE_RATE,
            "the capture is stored at the model rate"
        );
        assert_eq!(
            reader.samples::<i16>().count(),
            SAMPLE_RATE as usize,
            "the whole capture is stored"
        );
        assert_eq!(
            std::fs::read_to_string(take.join("raw/0.txt")).unwrap(),
            "first take",
            "window 0's pre-normalization text"
        );
        assert_eq!(
            std::fs::read_to_string(take.join("raw/1.txt")).unwrap(),
            "second take",
            "window 1's pre-normalization text"
        );
        assert_eq!(
            std::fs::read_to_string(take.join("joined.txt")).unwrap(),
            "first take second take",
            "joined.txt is the exact normalizer input"
        );
        assert_eq!(
            std::fs::read_to_string(take.join("text.txt")).unwrap(),
            "First take, second take.",
            "text.txt is the final normalized text"
        );
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(take.join("meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta["duration_ms"], 1000);
        assert_eq!(meta["processing_ms"], 7);
        assert_eq!(meta["truncated"], false);
        assert_eq!(meta["outcome"], "transcript");
        assert_eq!(meta["stages_ms"]["mel"], 0);
        assert_eq!(meta["windows"][0]["file"], "raw/0.txt");
        assert_eq!(meta["windows"][1]["end_ms"], 1000);
    }

    /// The store cannot grow without bound: past the retained count the
    /// oldest takes leave, by directory name.
    #[test]
    fn the_store_keeps_the_last_ten_takes() {
        let dir = tempfile::tempdir().unwrap();
        let audio = vec![0.5; 16];
        let stages = Stages::default();
        let windows = vec![];
        for id in 1..=12u128 {
            capture_take(dir.path(), id, &take_record(&audio, &windows, "", "", &stages));
        }
        let takes: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(takes.len(), RETAINED_TAKES, "twelve takes leave ten, got {takes:?}");
        assert!(
            !takes.contains(&"take-0000000000001".to_owned()),
            "the oldest take is pruned, got {takes:?}"
        );
        assert!(
            takes.contains(&"take-0000000000012".to_owned()),
            "the newest take survives, got {takes:?}"
        );
    }

    /// Diagnostics must never break a take: a directory that cannot be
    /// created is logged and dropped, not propagated.
    #[test]
    fn a_diagnostics_failure_is_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "a regular file, so subdirectories cannot exist").unwrap();
        let audio = vec![0.5; 16];
        let stages = Stages::default();
        let windows = vec![];
        capture_take(&blocker.join("under"), 1, &take_record(&audio, &windows, "", "", &stages));
        assert!(!blocker.is_dir(), "the unwritable location must not have been turned into one");
    }
}
