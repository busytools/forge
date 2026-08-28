//! Integrity gate over the committed fixture corpus. Needs no model
//! weights, no microphone and no pipeline, so it runs in CI in
//! milliseconds and catches a corrupted or half-added clip at check time
//! rather than later as a confusing transcript diff.
//!
//! Every property below has a negative control, so the gate is known to be
//! able to return a negative: recorded `sha256` matches the bytes on disk,
//! the manifest and the directory are in bijection, `duration_ms` matches
//! the clip's own WAV header, every clip carries the audio format the ASR
//! expects, no baseline is blank, and the bytes decode as audio at all.
//!
//! Three of those exist because `sha256` cannot see them, and that is the
//! point of having them: a clip in the wrong sample rate has a perfectly
//! valid hash, so does a manifest whose baseline was blanked by hand, and
//! so do bytes that are not audio. The duration check earns its place
//! separately: the bench divides by `duration_ms` to get a realtime
//! factor, so a wrong duration silently corrupts that number.
//!
//! `duration_ms` merely PARSING is enforced by deserialization, and so is
//! a RENAMED baseline key - a missing field is a deserialization error,
//! measured rather than assumed. The gap the blank-baseline check closes
//! is a key that is present with an empty value, which parses fine and
//! would leave the asserting half comparing real output against nothing.
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
//! catches a normalizer that starts MANGLING clean input, and it is FULLY
//! blind to one that quietly degrades into a PASSTHROUGH - bump s1-mini,
//! have it stop cleaning entirely, and 11 of 15 go green because a
//! passthrough is the correct answer on them. There is no clip that
//! catches that, and no exception to look for.
//!
//! # `15_020s.wav` is an ASR fixture, not a normalizer one
//!
//! Its ASR renders GGUF as "GG, UF" and that survives the whole pipeline,
//! which makes it an anchor for ASR drift: if the transcript changes
//! there, something moved. Leaving it unchanged is correct - s1-mini
//! normalizes styling, structure and context and does no vocabulary
//! reconstruction, so given "P Y torch" it returns "P-Y torch".
//!
//! The baselines are known-good rather than correct, so the bench reports
//! what changed for a human to read rather than grading it, and no
//! accuracy assertion belongs in CI.
//!
//! # Numbers discipline
//!
//! Speed figures are MEASURED and reproducible. Accuracy figures are
//! DIRECTIONAL - scored against Superwhisper's own output, partly
//! circular, one speaker, English, and over a SEPARATE 27-sample corpus
//! from Superwhisper history rather than these 15 clips. That split holds
//! anywhere either number is written down.

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

/// The format the ASR expects, and what every clip in the corpus is.
const EXPECTED_CHANNELS: u16 = 1;
const EXPECTED_SAMPLE_RATE: u32 = 16_000;
const EXPECTED_BITS_PER_SAMPLE: u16 = 16;

/// A `manifest.json` entry. `source_id` is the only field the gate does
/// not read, and serde ignores it.
#[derive(Debug, Deserialize)]
struct Entry {
    file: String,
    duration_ms: u64,
    sha256: String,
    baseline_asr: String,
    baseline_normalized: String,
}

/// A clip as the gate sees it: a name and its bytes.
struct Clip {
    name: String,
    bytes: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
enum Problem {
    ShaMismatch { file: String, recorded: String, actual: String },
    FileWithoutEntry { file: String },
    EntryWithoutFile { file: String },
    DurationDisagreesWithHeader { file: String, manifest_ms: u64, header_ms: u64 },
    UnexpectedFormat { file: String, channels: u16, sample_rate: u32, bits_per_sample: u16 },
    BlankBaseline { file: String, field: &'static str },
    UnreadableWav { file: String, error: String },
}

/// What the clip's own WAV header says about it.
struct WavProbe {
    ms: u64,
    channels: u16,
    sample_rate: u32,
    bits_per_sample: u16,
}

fn wav_probe(bytes: &[u8]) -> Result<WavProbe, String> {
    let reader = hound::WavReader::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let spec = reader.spec();
    if spec.sample_rate == 0 {
        return Err("sample rate is zero".to_owned());
    }
    Ok(WavProbe {
        ms: u64::from(reader.duration()) * 1000 / u64::from(spec.sample_rate),
        channels: spec.channels,
        sample_rate: spec.sample_rate,
        bits_per_sample: spec.bits_per_sample,
    })
}

/// The whole gate, as a pure function over parsed entries and clip bytes so
/// the negative controls need no filesystem.
fn check(entries: &[Entry], clips: &[Clip]) -> Vec<Problem> {
    let mut problems = Vec::new();

    let entry_names: BTreeSet<&str> = entries.iter().map(|e| e.file.as_str()).collect();
    let clip_names: BTreeSet<&str> = clips.iter().map(|c| c.name.as_str()).collect();

    for name in clip_names.difference(&entry_names) {
        problems.push(Problem::FileWithoutEntry { file: (*name).to_owned() });
    }
    for name in entry_names.difference(&clip_names) {
        problems.push(Problem::EntryWithoutFile { file: (*name).to_owned() });
    }

    for entry in entries {
        for (field, text) in [
            ("baseline_asr", &entry.baseline_asr),
            ("baseline_normalized", &entry.baseline_normalized),
        ] {
            if text.trim().is_empty() {
                problems.push(Problem::BlankBaseline { file: entry.file.clone(), field });
            }
        }

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

        match wav_probe(&clip.bytes) {
            Ok(probe) => {
                // One millisecond of slack: a frame count need not divide
                // the sample rate exactly, and a wrong duration is wrong
                // by hundreds of milliseconds rather than by one.
                if probe.ms.abs_diff(entry.duration_ms) > 1 {
                    problems.push(Problem::DurationDisagreesWithHeader {
                        file: entry.file.clone(),
                        manifest_ms: entry.duration_ms,
                        header_ms: probe.ms,
                    });
                }
                if probe.channels != EXPECTED_CHANNELS
                    || probe.sample_rate != EXPECTED_SAMPLE_RATE
                    || probe.bits_per_sample != EXPECTED_BITS_PER_SAMPLE
                {
                    problems.push(Problem::UnexpectedFormat {
                        file: entry.file.clone(),
                        channels: probe.channels,
                        sample_rate: probe.sample_rate,
                        bits_per_sample: probe.bits_per_sample,
                    });
                }
            }
            Err(error) => problems.push(Problem::UnreadableWav { file: entry.file.clone(), error }),
        }
    }

    problems
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn load_manifest() -> Vec<Entry> {
    let raw = std::fs::read_to_string(fixtures_dir().join("manifest.json")).unwrap();
    serde_json::from_str(&raw).unwrap()
}

fn load_clips() -> Vec<Clip> {
    let mut clips = Vec::new();
    for dirent in std::fs::read_dir(fixtures_dir()).unwrap() {
        let path = dirent.unwrap().path();
        if path.extension().is_some_and(|e| e == "wav") {
            clips.push(Clip {
                name: path.file_name().unwrap().to_string_lossy().into_owned(),
                bytes: std::fs::read(&path).unwrap(),
            });
        }
    }
    clips
}

/// A silent clip in an arbitrary format, one second long.
fn synth_wav(channels: u16, sample_rate: u32) -> Vec<u8> {
    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: EXPECTED_BITS_PER_SAMPLE,
        sample_format: hound::SampleFormat::Int,
    };
    let mut buf = Vec::new();
    {
        let mut writer = hound::WavWriter::new(Cursor::new(&mut buf), spec).unwrap();
        for _ in 0..(sample_rate * u32::from(channels)) {
            writer.write_sample(0i16).unwrap();
        }
        writer.finalize().unwrap();
    }
    buf
}

/// A one-second clip in the corpus format.
fn clip(name: &str) -> Clip {
    clip_in_format(name, EXPECTED_CHANNELS, EXPECTED_SAMPLE_RATE)
}

fn clip_in_format(name: &str, channels: u16, sample_rate: u32) -> Clip {
    Clip { name: name.to_owned(), bytes: synth_wav(channels, sample_rate) }
}

/// An entry that agrees with the clip on every property the gate checks.
fn entry_for(clip: &Clip, duration_ms: u64) -> Entry {
    Entry {
        file: clip.name.clone(),
        duration_ms,
        sha256: hex::encode(Sha256::digest(&clip.bytes)),
        baseline_asr: "spoken words".to_owned(),
        baseline_normalized: "spoken words".to_owned(),
    }
}

#[test]
fn corpus_matches_manifest() {
    let problems = check(&load_manifest(), &load_clips());
    assert!(problems.is_empty(), "committed fixture corpus is not intact: {problems:#?}");
}

#[test]
fn normalizer_coverage_is_still_what_the_docs_claim() {
    let exercising =
        load_manifest().iter().filter(|e| e.baseline_asr != e.baseline_normalized).count();

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
    let good = clip("01_003s.wav");
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
    let present = clip("02_004s.wav");
    let unregistered = clip("99_099s.wav");
    let mut absent = entry_for(&unregistered, 1000);
    absent.file = "98_098s.wav".to_owned();
    let entries = vec![entry_for(&present, 1000), absent];

    let problems = check(&entries, &[present, unregistered]);

    assert!(
        problems.contains(&Problem::FileWithoutEntry { file: "99_099s.wav".to_owned() }),
        "a wav on disk with no manifest entry must be reported: {problems:#?}"
    );
    assert!(
        problems.contains(&Problem::EntryWithoutFile { file: "98_098s.wav".to_owned() }),
        "a manifest entry with no wav on disk must be reported: {problems:#?}"
    );
}

#[test]
fn duration_disagreeing_with_wav_header_is_reported() {
    let one_second = clip("03_005s.wav");
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

/// Both clips here carry a valid sha256 and a duration matching their own
/// header, so format is the only property that can catch them - which is
/// why this check exists alongside the hash rather than being folded into
/// it.
#[test]
fn unexpected_audio_format_is_reported() {
    let stereo = clip_in_format("16_001s.wav", 2, EXPECTED_SAMPLE_RATE);
    let wrong_rate = clip_in_format("17_001s.wav", EXPECTED_CHANNELS, 44_100);
    let entries = vec![entry_for(&stereo, 1000), entry_for(&wrong_rate, 1000)];

    let problems = check(&entries, &[stereo, wrong_rate]);

    assert_eq!(
        problems,
        vec![
            Problem::UnexpectedFormat {
                file: "16_001s.wav".to_owned(),
                channels: 2,
                sample_rate: EXPECTED_SAMPLE_RATE,
                bits_per_sample: EXPECTED_BITS_PER_SAMPLE,
            },
            Problem::UnexpectedFormat {
                file: "17_001s.wav".to_owned(),
                channels: EXPECTED_CHANNELS,
                sample_rate: 44_100,
                bits_per_sample: EXPECTED_BITS_PER_SAMPLE,
            }
        ],
        "a clip whose channel count or sample rate is not what the ASR expects must be reported, \
         even though its sha256 and duration are both valid"
    );
}

/// `UnreadableWav` is the gate's only defence for a clip whose recorded
/// sha256 is correct over bytes that are not decodable audio: the duration
/// and format checks both live inside the `Ok` arm and never run. Two
/// paths reach it, and the second is worth having because hound ACCEPTS a
/// declared sample rate of zero - measured - so that guard is live rather
/// than defensive decoration.
#[test]
fn a_clip_that_is_not_decodable_audio_is_reported() {
    let not_audio = Clip { name: "16_001s.wav".to_owned(), bytes: b"this is not a wav".to_vec() };
    let entry = entry_for(&not_audio, 1000);

    let problems = check(&[entry], &[not_audio]);

    assert!(
        matches!(problems.as_slice(), [Problem::UnreadableWav { file, .. }] if file == "16_001s.wav"),
        "bytes that are not decodable audio must be reported; the sha256 over them is perfectly \
         valid, so nothing else in the gate can see it. got {problems:#?}"
    );
}

#[test]
fn a_clip_declaring_a_zero_sample_rate_is_reported() {
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend(b"RIFF");
    bytes.extend(&36u32.to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(&16u32.to_le_bytes());
    bytes.extend(&1u16.to_le_bytes()); // PCM
    bytes.extend(&1u16.to_le_bytes()); // mono
    bytes.extend(&0u32.to_le_bytes()); // sample rate, the point of this fixture
    bytes.extend(&0u32.to_le_bytes()); // byte rate
    bytes.extend(&2u16.to_le_bytes()); // block align
    bytes.extend(&16u16.to_le_bytes()); // bits per sample
    bytes.extend(b"data");
    bytes.extend(&0u32.to_le_bytes());

    let clip = Clip { name: "17_001s.wav".to_owned(), bytes };
    let entry = entry_for(&clip, 0);

    let problems = check(&[entry], &[clip]);

    assert!(
        matches!(
            problems.as_slice(),
            [Problem::UnreadableWav { file, error }]
                if file == "17_001s.wav" && error.contains("sample rate is zero")
        ),
        "a declared sample rate of zero must be reported rather than dividing by it; hound parses \
         such a header happily, so this guard is reachable. got {problems:#?}"
    );
}

#[test]
fn blank_baseline_is_reported() {
    let good = clip("05_006s.wav");
    let mut entry = entry_for(&good, 1000);
    entry.baseline_normalized = "   ".to_owned();

    let problems = check(&[entry], &[good]);

    assert_eq!(
        problems,
        vec![Problem::BlankBaseline {
            file: "05_006s.wav".to_owned(),
            field: "baseline_normalized",
        }],
        "a baseline that is present but blank must be reported, since the asserting half would \
         otherwise compare real output against nothing"
    );
}
