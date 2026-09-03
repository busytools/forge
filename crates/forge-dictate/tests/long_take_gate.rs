//! The long-take gate: a multi-minute dictation must land COMPLETE.
//!
//! Before windowing, a single-pass decode of this clip derailed into
//! repetition loops and skip-ahead re-syncs that dropped whole
//! paragraphs, with `truncated` false and nothing flagged.
//!
//! The clip is made with macOS `say`, so the whole file is macOS-only;
//! a crate-level cfg rather than one on the test keeps the helpers from
//! going dead-code on other platforms under `-D warnings`.
#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;

use forge_dictate::{Config, Engine, Outcome, Samples};
use sha2::{Digest, Sha256};

/// Dictation-style speech: ten distinct paragraphs, eight seconds of
/// pause between them, about four minutes all told - well past where a
/// single-pass decode derails. Paragraph openings double as the
/// completeness probes.
const SCRIPT: &str = "\
Okay, opening with the workspace layout. The crates are layered acyclically, primitives is a pure data leaf, the sdk speaks stream json to the claude subprocess, the agent owns env and cloud, the workspace orchestrates sessions, and the tui renders.
[[slnc 8000]]
Second paragraph, about the single instance guard. The flock lives on a machine local lockfile under app support, never on the config dir, because a sync daemon applying an incoming change rewrites files by rename and would swap the lock inode out from under a running forge.
[[slnc 8000]]
Third paragraph, config versus state. Forge toml is the only config file, it is read only and safe to sync, while crons, workers, usage cache and the spinner override live in one redb database that churns about once a minute and must never be synced.
[[slnc 8000]]
Fourth paragraph, the dictate crate. It is a leaf that owns model fetching, microphone capture at sixteen kilohertz, transcription with a cohere encoder decoder, and normalization with s one mini, and it may not depend on any other forge crate.
[[slnc 8000]]
Fifth paragraph, the capture cap. Thirty minutes is reserved eagerly at four bytes a sample because the audio callback must not allocate, an hour would be two hundred nineteen megabytes, and past the cap the recorder stops itself and flags the take as truncated.
[[slnc 8000]]
Sixth paragraph, the engine worker. One job runs at a time, weights load on the worker thread, cancellation is a token per job, teardown discards the backlog rather than draining it, and the join in drop keeps ggml from tripping its metal assert.
[[slnc 8000]]
Seventh paragraph, the normalizer. It rewrites raw recognition output into clean text using the whole sentence shape, a failure falls back to the raw words, and the budget is one point three times the prompt plus thirty two tokens.
[[slnc 8000]]
Eighth paragraph, the level meter. The read is take and reset, so each poll answers the loudest sample since the last read, which is what a bar per window wants, and the old all time peak froze the meter on the first syllable.
[[slnc 8000]]
Ninth paragraph, wire conformance. The baselines are live captures, replay guarantees every inbound line round trips through the decoder without unknown lines, and a committed capture carries whatever the capture machine printed.
[[slnc 8000]]
Tenth paragraph, closing. The release recipe bumps the workspace version and tags locally but never pushes, the book is deployed on every push to main, and a scoped change must not alter anything else observable, that is the whole walk through, thanks.
[[slnc 2000]]
";

const OPENERS: &[&str] = &[
    "opening with the workspace",
    "single instance guard",
    "config versus state",
    "the dictate crate",
    "capture cap",
    "engine worker",
    "the normalizer",
    "level meter",
    "wire conformance",
    "whole walk",
];

/// The clip, regenerated whenever the script changes. Recipe if you
/// would rather make it by hand: `say -v Samantha -r 145 -o clip.aiff -f script.txt`
/// then `afconvert -f WAVE -d LEI16@16000 -c 1 clip.aiff clip.wav`.
///
/// The wav lands via write-temp-then-rename: a run killed mid-convert
/// must not leave a truncated clip that every later run trusts.
fn ensure_clip() -> PathBuf {
    let tag = hex::encode(Sha256::digest(SCRIPT.as_bytes()))[..8].to_owned();
    let wav = std::env::temp_dir().join(format!("forge-dictate-gate-{tag}.wav"));
    if wav.exists() {
        return wav;
    }
    let text = std::env::temp_dir().join(format!("forge-dictate-gate-{tag}.txt"));
    let aiff = std::env::temp_dir().join(format!("forge-dictate-gate-{tag}.aiff"));
    let part = std::env::temp_dir().join(format!("forge-dictate-gate-{tag}.wav.part"));
    std::fs::write(&text, SCRIPT).expect("the say script must be writable");
    let said = std::process::Command::new("say")
        .args(["-v", "Samantha", "-r", "145", "-o"])
        .arg(&aiff)
        .arg("-f")
        .arg(&text)
        .status()
        .is_ok_and(|s| s.success());
    let converted = said
        && std::process::Command::new("afconvert")
            .args(["-f", "WAVE", "-d", "LEI16@16000", "-c", "1"])
            .arg(&aiff)
            .arg(&part)
            .status()
            .is_ok_and(|s| s.success());
    if !converted {
        let _ = std::fs::remove_file(&part);
        panic!(
            "the gate clip could not be made: `say`/`afconvert` are unavailable or failed, \
             and a gate that runs nothing must not read green. Make the clip by hand \
             (recipe in this module's comment) or restore the tools."
        );
    }
    std::fs::rename(&part, &wav).expect("the finished clip must rename into place");
    wav
}

/// The gate: every paragraph must reach the recognition, the take must
/// not be flagged partial, and the per-window progress must count
/// every window in order. Runs the shipped configuration, normalizer
/// and all.
#[test]
#[ignore = "needs the ASR weights; generates a ~4 min `say` clip on first run"]
#[cfg(target_os = "macos")]
fn a_long_take_lands_complete_with_window_progress() {
    for spec in [Config::default().asr_model, Config::default().normalizer.expect("shipped")] {
        let path =
            dirs::cache_dir().map(|d| d.join("forge-dictate").join(&spec.file)).expect("cache");
        assert!(
            path.exists(),
            "{} is not on disk at {}. This gate does not fetch - run prepare() first.",
            spec.file,
            path.display()
        );
    }
    let wav = ensure_clip();

    let mut reader =
        hound::WavReader::new(std::fs::File::open(&wav).expect("the clip must be readable"))
            .expect("the clip must parse");
    let pcm: Vec<f32> =
        reader.samples::<i16>().map(|s| f32::from(s.expect("a sample")) / 32768.0).collect();
    #[allow(clippy::cast_precision_loss)]
    let audio = pcm.len() as f64 / f64::from(forge_dictate::SAMPLE_RATE);
    eprintln!("gate clip: {audio:.1}s");

    let engine = Engine::new(Config::default()).expect("engine must start");
    engine.wait_ready().expect("the weights must load");
    let mut ticket = engine.transcribe(Samples::mono(pcm)).expect("queued");
    let progress = ticket.take_progress().expect("the ticket carries a progress stream");
    let outcome = ticket.recv().expect("the take must be answered");

    let Outcome::Transcript(transcript) = outcome else {
        panic!("a spoken take must not read as silence: {outcome:?}");
    };
    assert!(!transcript.truncated, "no window may outrun its decode budget");
    assert!(
        transcript.stages.audio.as_secs_f64() > 200.0,
        "the gate clip must actually be a long take, got {:?}",
        transcript.stages.audio
    );

    // Progress: a take this long spans several windows; the steps must
    // count every window in order, against one constant total.
    let mut steps = Vec::new();
    while let Ok(step) = progress.try_recv() {
        steps.push(step);
    }
    assert!(!steps.is_empty(), "the take must report its windows");
    let total = steps[0].total;
    assert!(total > 1, "a {audio:.0}s take must span several windows, got total {total}");
    assert!(
        steps.iter().all(|s| s.total == total),
        "the total must hold across the take, got {steps:?}"
    );
    for (i, step) in steps.iter().enumerate() {
        assert_eq!(step.window, i + 1, "steps must count the windows in order");
    }
    assert_eq!(steps.last().expect("steps").window, total, "the last step is the last window");

    // Completeness, on the recognition and the normalized text alike:
    // the normalizer reads the joined take once, so both must carry
    // every paragraph.
    let asr_lower = transcript.asr.to_lowercase();
    let text_lower = transcript.text.to_lowercase();
    for opener in OPENERS {
        assert!(
            asr_lower.contains(opener),
            "the recognition must carry the paragraph at {opener:?} - words were lost"
        );
        assert!(
            text_lower.contains(opener),
            "the normalized text must carry the paragraph at {opener:?}"
        );
    }
    eprintln!(
        "gate: {} windows, asr {} words, text {} words - complete",
        total,
        transcript.asr.split_whitespace().count(),
        transcript.text.split_whitespace().count()
    );
}
