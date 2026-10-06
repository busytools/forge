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
    /// Engine accept to first window start: the queue, and for early
    /// takes the tail of the model load. The felt lag the stages do
    /// not carry; `start_lag_ms` + `processing_ms` spans accept to
    /// reply.
    pub(crate) start_lag_ms: u64,
    pub(crate) processing_ms: u64,
    pub(crate) truncated: bool,
    /// `transcript`, `empty` or `recognition_error`.
    pub(crate) outcome: &'a str,
    /// The window whose recognition failed and the error text, when
    /// the take ended in a recognition error.
    pub(crate) recognition_error: Option<(usize, String)>,
}

/// How many takes the store keeps, and the bench's corpus along with it.
///
/// Measured on the maintainer's live store (2026-10-06, ten takes,
/// read-only): 10.3 MB in total, median 226 KB, mean 1.03 MB, worst
/// 7.68 MB. 400 takes is around 90 MB typical and about 3 GB if every
/// take were the worst case, which is the price of a shelf wide enough
/// for a bench to score against.
const RETAINED_TAKES: usize = 400;

/// The unix-millisecond stamp a take directory is named by, as
/// `take-<13 digits>`: sortable, and 13 digits holds until the year
/// 2286 so lexicographic order never lies about recency.
pub(crate) fn take_stamp() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis())
}

/// The file a host arms the read-aloud set with: its contents are the
/// passage, and the NEXT finished take consumes it.
const READ_ALOUD_ARMED: &str = "armed.txt";

/// Write one take into `dir/take-<take_id>/` and prune the store to
/// [`RETAINED_TAKES`]. Nothing here can fail the caller: every step
/// logs its own failure and stops that take's capture.
///
/// `read_aloud` is the set directory when this host has one configured:
/// while that directory carries the armed marker, THIS take - the one
/// being captured, never a neighbour's - is also stored as the set, and
/// the marker goes with it so exactly one take answers one arming.
pub(crate) fn capture_take(
    dir: &Path,
    take_id: u128,
    take: &TakeRecord<'_>,
    read_aloud: Option<&Path>,
) {
    let take_dir = dir.join(format!("take-{take_id:013}"));
    // meta.json is written LAST, which is what makes a take directory
    // without it incomplete: an early return above leaves a partial
    // directory that still counts for retention, but never reads as a
    // complete take.
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
    let mut meta = json!({
        "duration_ms": ms(take.stages.audio),
        // Accept to first window start, so the wall total reconciles:
        // start_lag_ms + processing_ms spans the engine's accept-to-reply.
        "start_lag_ms": take.start_lag_ms,
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
    if let Some((window, error)) = &take.recognition_error {
        meta["recognition_error"] = json!({ "window": window, "error": error });
    }
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

    // The set copy comes after the meta: a set take the store cannot read
    // as complete (meta lands last, and the copy needs it) is a gold set
    // the bench would skip.
    if let Some(set_dir) = read_aloud {
        store_read_aloud(set_dir, &take_dir, take);
    }

    prune(dir);
}

/// Store one finished take as the read-aloud set, when the set directory
/// carries the armed marker.
///
/// The set REPLACES whatever stood there: the previous passage, the
/// previous take, and one entry naming the wav's own sha256 so the corpus
/// a result was measured on can be named later. Best-effort like the rest
/// of this module - a failure here must never touch the take itself.
fn store_read_aloud(set_dir: &Path, take_dir: &Path, take: &TakeRecord<'_>) {
    let marker = set_dir.join(READ_ALOUD_ARMED);
    let Ok(passage) = std::fs::read_to_string(&marker) else {
        return;
    };
    if let Err(error) = std::fs::create_dir_all(set_dir) {
        tracing::warn!(%error, dir = %set_dir.display(), "read-aloud: set directory not writable");
        return;
    }
    // The old set goes first, so a half-written replacement never leaves
    // two takes under one passage.
    for entry in std::fs::read_dir(set_dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("take-") {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }

    let stored = set_dir.join(take_dir.file_name().unwrap_or_default());
    if let Err(error) = copy_dir(take_dir, &stored) {
        tracing::warn!(%error, from = %take_dir.display(), "read-aloud: the take could not be copied");
        return;
    }
    if let Err(error) = std::fs::write(set_dir.join("passage.txt"), &passage) {
        tracing::warn!(%error, dir = %set_dir.display(), "read-aloud: the passage was not written");
        return;
    }
    let digest = sha256_file(&stored.join("output.wav"));
    let manifest = json!({
        "sha256": digest,
        "take": take_dir.file_name().map(|name| name.to_string_lossy().into_owned()),
        "outcome": take.outcome,
    });
    match serde_json::to_vec_pretty(&manifest) {
        Ok(bytes) => {
            if let Err(error) = std::fs::write(set_dir.join("manifest.json"), bytes) {
                tracing::warn!(%error, dir = %set_dir.display(), "read-aloud: the manifest was not written");
                return;
            }
        }
        Err(error) => {
            tracing::warn!(%error, "read-aloud: the manifest is not serializable");
            return;
        }
    }
    // Consumed: exactly one take answers one arming.
    if let Err(error) = std::fs::remove_file(&marker) {
        tracing::warn!(%error, path = %marker.display(), "read-aloud: the arming marker was not cleared");
    }
}

/// Copy one take directory whole - the wav, the texts, the meta - so the
/// set holds the same bytes the store does.
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let path = entry.path();
        let target = to.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &target)?;
        } else {
            std::fs::copy(&path, &target)?;
        }
    }
    Ok(())
}

/// A file's sha256, lowercase hex; empty when it cannot be read.
fn sha256_file(path: &Path) -> String {
    use sha2::Digest as _;
    let Ok(bytes) = std::fs::read(path) else {
        return String::new();
    };
    hex::encode(sha2::Sha256::digest(&bytes))
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
        // Safe after the clamp: the value is inside i16's range.
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let value = (sample * 32767.0).clamp(-32768.0, 32767.0).round() as i16;
        writer.write_sample(value).map_err(|error| error.to_string())?;
    }
    writer.finalize().map_err(|error| error.to_string())
}

/// Delete every take but the newest [`RETAINED_TAKES`]. Only store
/// directories count - `take-` followed by nothing but digits - so a
/// user directory that happens to share the prefix is never touched.
/// Removal is best-effort, and a directory that will not leave is
/// logged: it occupies a retention slot invisibly otherwise.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut takes: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.file_name().is_some_and(starts_take))
        .collect();
    takes.sort();
    for stale in takes.iter().rev().skip(RETAINED_TAKES) {
        if let Err(error) = std::fs::remove_dir_all(stale) {
            tracing::warn!(%error, dir = %stale.display(), "diagnostics: stale take not pruned");
        }
    }
}

/// A store directory: `take-` followed by all-ASCII digits. Anything
/// else - `take-2026-photos`, a user's own folder - is not ours to
/// prune.
fn starts_take(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|s| {
        let id = s.strip_prefix("take-").unwrap_or_default();
        !id.is_empty() && id.len() >= 13 && id.bytes().all(|b| b.is_ascii_digit())
    })
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
            start_lag_ms: 3,
            processing_ms: 7,
            truncated: false,
            outcome: "transcript",
            recognition_error: None,
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
            None,
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
        assert_eq!(meta["start_lag_ms"], 3, "the pre-transcribe lag reaches the metadata");
        assert_eq!(meta["processing_ms"], 7);
        assert_eq!(meta["truncated"], false);
        assert_eq!(meta["outcome"], "transcript");
        assert_eq!(meta["stages_ms"]["mel"], 0);
        assert_eq!(meta["windows"][0]["file"], "raw/0.txt");
        assert_eq!(meta["windows"][1]["end_ms"], 1000);
        assert!(
            meta.get("recognition_error").is_none(),
            "a successful take names no failed window"
        );
    }

    /// A recognition-error take names the window that failed and the
    /// error text, so meta.json answers "which stage ate the words"
    /// for the one outcome whose transcript stages cannot.
    #[test]
    fn a_recognition_error_take_records_the_failed_window() {
        let dir = tempfile::tempdir().unwrap();
        let audio = vec![0.5; 16];
        let stages = Stages::default();
        let windows = vec![WindowRecord { start_ms: 0, end_ms: 500, raw: "first".into() }];
        let mut record = take_record(&audio, &windows, "first", "", &stages);
        record.outcome = "recognition_error";
        record.recognition_error = Some((1, "decode failed: bad input".into()));
        capture_take(dir.path(), 7, &record, None);

        let meta: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("take-0000000000007/meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(meta["outcome"], "recognition_error");
        assert_eq!(meta["recognition_error"]["window"], 1, "the failed window index");
        assert_eq!(
            meta["recognition_error"]["error"], "decode failed: bad input",
            "the recognition failure text"
        );
    }

    /// **The read-aloud set.** While the set directory is armed, the next
    /// take stored is copied beside the passage as the set - and it is
    /// THIS take that answers the arming, however many were stored before
    /// it. A second arming replaces the set rather than stacking one.
    #[test]
    fn an_armed_set_takes_the_next_take_and_a_later_arming_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        let set = tempfile::tempdir().unwrap();
        let audio = vec![0.5; 16];
        let stages = Stages::default();
        let windows = vec![];

        // Not armed: the take lands in the store and nowhere else.
        capture_take(
            dir.path(),
            1,
            &take_record(&audio, &windows, "one", "one", &stages),
            Some(set.path()),
        );
        assert!(!set.path().join("passage.txt").exists(), "nothing was armed");

        // Armed: the next take is the set, and the arming is consumed.
        std::fs::write(set.path().join("armed.txt"), "read this").unwrap();
        capture_take(
            dir.path(),
            2,
            &take_record(&audio, &windows, "two", "two", &stages),
            Some(set.path()),
        );
        assert_eq!(std::fs::read_to_string(set.path().join("passage.txt")).unwrap(), "read this");
        let kept = set.path().join("take-0000000000002");
        assert!(kept.join("output.wav").is_file(), "the take that answered the arming");
        assert!(
            kept.join("meta.json").is_file(),
            "complete, because the bench's read skips a take without its meta"
        );
        assert!(!set.path().join("armed.txt").exists(), "one take answers one arming");

        // Read back the way the bench reads it.
        let clip = crate::bench::read_aloud(set.path())
            .expect("the set reads")
            .expect("a passage and a take are there");
        assert_eq!(clip.truth.as_deref(), Some("read this"));
        assert_eq!(clip.audio.len(), audio.len());

        // Armed again: the set REPLACES, it does not stack.
        std::fs::write(set.path().join("armed.txt"), "read this too").unwrap();
        capture_take(
            dir.path(),
            3,
            &take_record(&audio, &windows, "three", "three", &stages),
            Some(set.path()),
        );
        assert_eq!(
            std::fs::read_to_string(set.path().join("passage.txt")).unwrap(),
            "read this too"
        );
        assert!(!kept.exists(), "the previous take leaves with the previous set");
        assert!(set.path().join("take-0000000000003").is_dir());
        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(set.path().join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert!(
            manifest["sha256"].as_str().is_some_and(|sha| sha.len() == 64),
            "the manifest names the wav's own sha, got {manifest}"
        );
    }

    /// Prune only ever touches store directories: `take-` followed by
    /// nothing but digits. A user folder sharing the prefix is not ours
    /// to remove, whatever its age.
    #[test]
    fn prune_never_touches_a_foreign_directory() {
        let dir = tempfile::tempdir().unwrap();
        let foreign = dir.path().join("take-2026-photos");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("holiday.jpg"), "not ours").unwrap();
        let stages = Stages::default();
        let audio = vec![0.5; 16];
        let windows = vec![];
        for id in 1..=RETAINED_TAKES as u128 + 1 {
            capture_take(dir.path(), id, &take_record(&audio, &windows, "", "", &stages), None);
        }
        assert!(foreign.is_dir(), "a user directory sharing the take- prefix must survive pruning");
        let takes: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("take-0"))
            .collect();
        assert_eq!(
            takes.len(),
            RETAINED_TAKES,
            "the store kept a different number than its own cap, got {takes:?}"
        );
    }

    /// The store cannot grow without bound: past the retained count the
    /// oldest takes leave, by directory name. **The cap is the knob, so
    /// this is a test of the pruning, not of the number** - it writes
    /// one past whatever `RETAINED_TAKES` is and asserts the survivors.
    #[test]
    fn the_store_keeps_the_last_of_its_retention() {
        let dir = tempfile::tempdir().unwrap();
        let audio = vec![0.5; 16];
        let stages = Stages::default();
        let windows = vec![];
        let newest = RETAINED_TAKES as u128 + 2;
        for id in 1..=newest {
            capture_take(dir.path(), id, &take_record(&audio, &windows, "", "", &stages), None);
        }
        let takes: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            takes.len(),
            RETAINED_TAKES,
            "{newest} takes leave the cap's number, got {takes:?}"
        );
        assert!(
            !takes.contains(&"take-0000000000001".to_owned()),
            "the oldest take is pruned, got {takes:?}"
        );
        assert!(
            takes.contains(&format!("take-{newest:013}")),
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
        capture_take(
            &blocker.join("under"),
            1,
            &take_record(&audio, &windows, "", "", &stages),
            None,
        );
        assert!(!blocker.is_dir(), "the unwritable location must not have been turned into one");
    }
}
