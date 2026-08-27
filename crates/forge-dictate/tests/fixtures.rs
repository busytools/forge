//! Integrity gate over the committed fixture corpus. Needs no model
//! weights, no microphone and no pipeline, so it runs in CI in
//! milliseconds and catches a corrupted or half-added clip at check time
//! rather than later as a confusing transcript diff.
//!
//! Three properties, each with a negative control below so the gate is
//! known to be able to return a negative: recorded `sha256` matches the
//! bytes on disk, the manifest and the directory are in bijection, and
//! `duration_ms` matches the clip's own WAV header. That last one is what
//! makes the duration trustworthy as the denominator of a realtime
//! factor; `duration_ms` merely PARSING is already enforced by
//! deserialization, so asserting it would be vacuous.
//!
//! # The baselines are locked known-good, not ground truth
//!
//! `baseline_asr` and `baseline_normalized` are another model's output
//! (Superwhisper's), not a human transcription. Their only job is drift
//! detection: if a dependency bump changes a transcript, the diff is the
//! signal. **Never tune our model to match them more closely** - that
//! optimises toward another model's errors. When output diverges, decide
//! whether ours got worse or merely different, then either fix the
//! regression or re-lock the baseline deliberately with a note saying why.
//!
//! # What this corpus cannot see, for whoever bumps a dependency
//!
//! Only 4 of the 15 clips exercise the normalizer at all; the other 11 are
//! correct no-ops where the ASR output was already clean. So the corpus is
//! a strong ASR regression gate and an ASYMMETRIC normalizer gate. It
//! catches a normalizer that starts MANGLING clean input. It is close to
//! blind to one that quietly degrades into a PASSTHROUGH - bump s1-mini,
//! have it stop cleaning entirely, and this corpus mostly goes green.
//!
//! `15_020s.wav` is the most valuable single fixture and the only clip
//! whose failure is unambiguous: its ASR renders GGUF as "GG, UF", which
//! is exactly the repair the normalizer exists to perform. Note the locked
//! `baseline_normalized` preserves that error, so byte-equality against
//! the baseline is the WRONG assertion for this one clip - see
//! `NORMALIZER_EXERCISING_CLIPS`.
//!
//! # Numbers discipline
//!
//! Speed figures are MEASURED and reproducible. Accuracy figures are
//! DIRECTIONAL - scored against Superwhisper's own output, partly
//! circular, 27 samples, one speaker, English. That split holds anywhere
//! either number is written down.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeSet;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

/// Clips whose `baseline_asr` and `baseline_normalized` differ, i.e. the
/// ones that actually exercise the normalizer. Checked rather than only
/// written in prose so the coverage limit above cannot rot silently when
/// somebody adds a clip.
const NORMALIZER_EXERCISING_CLIPS: usize = 4;

/// A `manifest.json` entry. Only the fields the gate reads are declared;
/// serde ignores `source_id` and the two baselines.
#[derive(Debug, Deserialize)]
struct Entry {
    file: String,
    duration_ms: u64,
    sha256: String,
}

/// A clip as the gate sees it: a name and its bytes.
struct Clip {
    name: String,
    bytes: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
enum Problem {
    ShaMismatch {
        file: String,
        recorded: String,
        actual: String,
    },
    FileWithoutEntry {
        file: String,
    },
    EntryWithoutFile {
        file: String,
    },
    DurationDisagreesWithHeader {
        file: String,
        manifest_ms: u64,
        header_ms: u64,
    },
    UnreadableWav {
        file: String,
        error: String,
    },
}

/// Milliseconds of audio the clip's own WAV header describes.
fn header_ms(bytes: &[u8]) -> Result<u64, String> {
    let reader = hound::WavReader::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let rate = u64::from(reader.spec().sample_rate);
    if rate == 0 {
        return Err("sample rate is zero".to_owned());
    }
    Ok(u64::from(reader.duration()) * 1000 / rate)
}

/// The whole gate, as a pure function over parsed entries and clip bytes so
/// the negative controls need no filesystem.
fn check(entries: &[Entry], clips: &[Clip]) -> Vec<Problem> {
    let mut problems = Vec::new();

    let entry_names: BTreeSet<&str> = entries.iter().map(|e| e.file.as_str()).collect();
    let clip_names: BTreeSet<&str> = clips.iter().map(|c| c.name.as_str()).collect();

    for name in clip_names.difference(&entry_names) {
        problems.push(Problem::FileWithoutEntry {
            file: (*name).to_owned(),
        });
    }
    for name in entry_names.difference(&clip_names) {
        problems.push(Problem::EntryWithoutFile {
            file: (*name).to_owned(),
        });
    }

    for entry in entries {
        let Some(clip) = clips.iter().find(|c| c.name == entry.file) else {
            continue;
        };

        let actual = hex::encode(Sha256::digest(&clip.bytes));
        if actual != entry.sha256 {
            problems.push(Problem::ShaMismatch {
                file: entry.file.clone(),
                recorded: entry.sha256.clone(),
                actual,
            });
        }

        match header_ms(&clip.bytes) {
            // One millisecond of slack: a frame count need not divide the
            // sample rate exactly, and a wrong duration is wrong by
            // hundreds of milliseconds rather than by one.
            Ok(ms) => {
                if ms.abs_diff(entry.duration_ms) > 1 {
                    problems.push(Problem::DurationDisagreesWithHeader {
                        file: entry.file.clone(),
                        manifest_ms: entry.duration_ms,
                        header_ms: ms,
                    });
                }
            }
            Err(error) => problems.push(Problem::UnreadableWav {
                file: entry.file.clone(),
                error,
            }),
        }
    }

    problems
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn load_corpus() -> (Vec<Entry>, Vec<Clip>) {
    let dir = fixtures_dir();
    let manifest = std::fs::read_to_string(dir.join("manifest.json")).unwrap();
    let entries: Vec<Entry> = serde_json::from_str(&manifest).unwrap();

    let mut clips = Vec::new();
    for dirent in std::fs::read_dir(&dir).unwrap() {
        let path = dirent.unwrap().path();
        if path.extension().is_some_and(|e| e == "wav") {
            clips.push(Clip {
                name: path.file_name().unwrap().to_string_lossy().into_owned(),
                bytes: std::fs::read(&path).unwrap(),
            });
        }
    }
    (entries, clips)
}

/// A minimal mono 16-bit 16 kHz clip, matching the corpus format.
fn synth_wav(frames: u32) -> Vec<u8> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut buf = Vec::new();
    {
        let mut writer = hound::WavWriter::new(Cursor::new(&mut buf), spec).unwrap();
        for _ in 0..frames {
            writer.write_sample(0i16).unwrap();
        }
        writer.finalize().unwrap();
    }
    buf
}

fn entry_for(clip: &Clip, duration_ms: u64) -> Entry {
    Entry {
        file: clip.name.clone(),
        duration_ms,
        sha256: hex::encode(Sha256::digest(&clip.bytes)),
    }
}

fn clip(name: &str, frames: u32) -> Clip {
    Clip {
        name: name.to_owned(),
        bytes: synth_wav(frames),
    }
}

#[test]
fn corpus_matches_manifest() {
    let (entries, clips) = load_corpus();
    let problems = check(&entries, &clips);
    assert!(
        problems.is_empty(),
        "committed fixture corpus is not intact: {problems:#?}"
    );
}

#[test]
fn normalizer_coverage_is_still_what_the_docs_claim() {
    #[derive(Deserialize)]
    struct Baselines {
        baseline_asr: String,
        baseline_normalized: String,
    }

    let manifest = std::fs::read_to_string(fixtures_dir().join("manifest.json")).unwrap();
    let rows: Vec<Baselines> = serde_json::from_str(&manifest).unwrap();
    let exercising = rows
        .iter()
        .filter(|r| r.baseline_asr != r.baseline_normalized)
        .count();

    assert_eq!(
        exercising, NORMALIZER_EXERCISING_CLIPS,
        "normalizer coverage drifted: docs claim {NORMALIZER_EXERCISING_CLIPS} clips exercise the \
         normalizer, the manifest has {exercising}. Update all four: NORMALIZER_EXERCISING_CLIPS, \
         this file's header, the coverage section of fixtures/README.md, and - if you added or \
         removed a clip - that README's opening clip count, total duration and corpus size"
    );
}

#[test]
fn sha_mismatch_is_reported() {
    let good = clip("01_003s.wav", 16_000);
    let real_sha = hex::encode(Sha256::digest(&good.bytes));
    let mut entry = entry_for(&good, 1000);
    entry.sha256 = "0".repeat(64);

    let problems = check(&[entry], &[good]);

    assert_eq!(
        problems,
        vec![Problem::ShaMismatch {
            file: "01_003s.wav".to_owned(),
            recorded: "0".repeat(64),
            actual: real_sha,
        }],
        "a recorded sha256 that does not match the bytes must be reported"
    );
}

#[test]
fn orphans_are_reported_in_both_directions() {
    let present = clip("02_004s.wav", 16_000);
    let unregistered = clip("99_099s.wav", 16_000);
    let entries = vec![
        entry_for(&present, 1000),
        Entry {
            file: "98_098s.wav".to_owned(),
            duration_ms: 1000,
            sha256: "0".repeat(64),
        },
    ];

    let problems = check(&entries, &[present, unregistered]);

    assert!(
        problems.contains(&Problem::FileWithoutEntry {
            file: "99_099s.wav".to_owned()
        }),
        "a wav on disk with no manifest entry must be reported: {problems:#?}"
    );
    assert!(
        problems.contains(&Problem::EntryWithoutFile {
            file: "98_098s.wav".to_owned()
        }),
        "a manifest entry with no wav on disk must be reported: {problems:#?}"
    );
}

#[test]
fn duration_disagreeing_with_wav_header_is_reported() {
    let one_second = clip("03_005s.wav", 16_000);
    let entry = entry_for(&one_second, 5328);

    let problems = check(&[entry], &[one_second]);

    assert_eq!(
        problems,
        vec![Problem::DurationDisagreesWithHeader {
            file: "03_005s.wav".to_owned(),
            manifest_ms: 5328,
            header_ms: 1000,
        }],
        "a duration_ms that disagrees with the clip's own WAV header must be reported"
    );
}
