//! The dictate gates: end-to-end runs against the real shipped
//! weights, ignored by default because they need 1.5 GB of models and
//! a voice to speak with. The clip is made with macOS `say`, so the
//! whole module is macOS-only; the file-level cfg rather than one per
//! test keeps the helpers from going dead-code on other platforms
//! under `-D warnings`.
//!
//! ```bash
//! cargo run -p forge-dictate --release --example fetch
//! cargo nextest run -p forge-dictate --release --run-ignored all -E 'test(gate)'
//! ```
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::{Config, ConfigBuilder, Engine, Outcome, Samples};
use sha2::{Digest, Sha256};

/// Dictation-style speech: ten distinct paragraphs, eight seconds of
/// pause between them, about four minutes all told - well past where a
/// single-pass decode derails. Paragraph openings double as the
/// completeness probes.
const LONG_SCRIPT: &str = "\
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

const LONG_OPENERS: &[&str] = &[
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

/// Four paragraphs with eight second pauses, about a hundred and ten
/// seconds - enough to cross the window ceiling twice, so the take
/// pipelines two segments and a tail. Openings double as the
/// join-completeness probes: the first and second land in one segment,
/// the rest in the pieces after it, so every landed text proves the
/// join carried all of them.
const PIPELINE_SCRIPT: &str = "\
Okay, first paragraph, the segmenter. A long take is cut at the quietest pause before it reaches the window ceiling, and each segment is queued for recognition while the microphone is still recording, so the words for what was just said are already done before the speaker stops.
[[slnc 8000]]
Second paragraph, the join. Every segment keeps its raw recognition text, and at the stop the raw texts are joined and normalized exactly once, so a sentence crossing a cut is repaired with the context around it and nothing is ever rewritten twice.
[[slnc 8000]]
Third paragraph, the tally. While the speaker talks, the status row counts the segments that have settled, and the total appears only when the take is stopped, because a live recording cannot know how many pieces it will produce.
[[slnc 8000]]
Fourth paragraph, the tail. When the speaker stops, only the last unfinished stretch still owes a transcript, so the wait after the stop is a second or two rather than the whole take, that is the whole walk through, thanks.
[[slnc 2000]]
";

/// `the tail` is probed loosely: the recognizer spells it `the tale`.
const PIPELINE_OPENERS: &[&str] = &[
    "first paragraph, the segmenter",
    "second paragraph, the join",
    "third paragraph, the tally",
    "fourth paragraph, the",
];

/// The clip for `script`, regenerated whenever the script changes.
/// `label` separates the gates' files so two gates running in
/// parallel cannot rename each other's clip out from under them.
/// Recipe if you would rather make one by hand:
/// `say -v Samantha -r 145 -o clip.aiff -f script.txt`
/// then `afconvert -f WAVE -d LEI16@16000 -c 1 clip.aiff clip.wav`.
///
/// The wav lands via write-temp-then-rename: a run killed mid-convert
/// must not leave a truncated clip that every later run trusts.
fn ensure_clip(label: &str, script: &str) -> PathBuf {
    let tag = hex::encode(Sha256::digest(script.as_bytes()))[..8].to_owned();
    let wav = std::env::temp_dir().join(format!("forge-dictate-gate-{label}-{tag}.wav"));
    if wav.exists() {
        return wav;
    }
    let text = std::env::temp_dir().join(format!("forge-dictate-gate-{label}-{tag}.txt"));
    let aiff = std::env::temp_dir().join(format!("forge-dictate-gate-{label}-{tag}.aiff"));
    let part = std::env::temp_dir().join(format!("forge-dictate-gate-{label}-{tag}.wav.part"));
    std::fs::write(&text, script).expect("the say script must be writable");
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

fn read_clip(label: &str, script: &str) -> Vec<f32> {
    let wav = ensure_clip(label, script);
    let mut reader =
        hound::WavReader::new(std::fs::File::open(&wav).expect("the clip must be readable"))
            .expect("the clip must parse");
    reader.samples::<i16>().map(|s| f32::from(s.expect("a sample")) / 32768.0).collect()
}

fn shipped_weights_on_disk() {
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
}

/// The long-take gate: a multi-minute take must land COMPLETE over the
/// direct path. Before windowing, a single-pass decode of this clip
/// derailed into repetition loops and skip-ahead re-syncs that dropped
/// whole paragraphs, with `truncated` false and nothing flagged.
#[test]
#[ignore = "needs the ASR weights; generates a ~4 min `say` clip on first run"]
fn a_long_take_gate_lands_complete() {
    shipped_weights_on_disk();
    let pcm = read_clip("long", LONG_SCRIPT);
    #[allow(clippy::cast_precision_loss)]
    let audio = pcm.len() as f64 / f64::from(crate::SAMPLE_RATE);
    eprintln!("gate clip: {audio:.1}s");

    let engine = Engine::new(Config::default()).expect("engine must start");
    engine.wait_ready().expect("the weights must load");
    let ticket = engine.transcribe(Samples::mono(pcm)).expect("queued");
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
    let asr_lower = transcript.asr.to_lowercase();
    let text_lower = transcript.text.to_lowercase();
    for opener in LONG_OPENERS {
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
        "gate: asr {} words, text {} words - complete",
        transcript.asr.split_whitespace().count(),
        transcript.text.split_whitespace().count()
    );
}

/// The pipelining gate: over the capture path, a segment must
/// transcribe WHILE the recording is still open, the landed text must
/// carry every paragraph across the join, and the landed text must be
/// exactly one normalizer pass over the joined raw - not per-segment
/// rewrites, which is the failure that makes pipelined text visibly
/// wrong.
#[test]
#[ignore = "needs the ASR weights; generates a ~90 s `say` clip on first run"]
fn a_pipelined_take_gate_transcribes_while_recording() {
    shipped_weights_on_disk();
    let pcm = read_clip("pipeline", PIPELINE_SCRIPT);
    #[allow(clippy::cast_precision_loss)]
    let audio = pcm.len() as f64 / f64::from(crate::SAMPLE_RATE);
    eprintln!("gate clip: {audio:.1}s");
    assert!(audio > 70.0, "the clip must cross the window ceiling, got {audio:.0}s");

    let diagnostics = tempfile::tempdir().unwrap();
    let cfg = ConfigBuilder::new().diagnostics_dir(diagnostics.path()).build();
    let engine = crate::test_support::engine_with_feeding_microphone(
        cfg,
        Arc::new(pcm),
        // Ten times faster than speech, still slow enough that a
        // segment finishing means genuine overlap, not a feed that
        // ended before the first cut.
        Duration::from_millis(100),
    )
    .expect("engine must start");
    engine.wait_ready().expect("the weights must load");
    let mut capture = engine.try_capture("pipeline-gate").expect("the mic must be held");
    let progress = capture.take_progress().expect("the capture carries a progress stream");
    let recording_started = Instant::now();

    // The overlap proof: a settled segment while the take is live.
    let mut steps = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        while let Ok(step) = progress.try_recv() {
            steps.push(step);
        }
        if steps.iter().any(|step| step.done >= 1) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no segment settled while the recording was open: {steps:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        steps.iter().all(|step| step.total.is_none()),
        "a live recording cannot know its total, got {steps:?}"
    );
    eprintln!("gate: segment settled during recording after {steps:?}");

    // Finish only once the whole clip is in: at ten times real time
    // the feed needs about a tenth of the clip's span, and finishing
    // early would stop the microphone with paragraphs still unspoken.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let feed_duration = Duration::from_millis((audio * 100.0) as u64) + Duration::from_secs(2);
    while recording_started.elapsed() < feed_duration {
        while let Ok(step) = progress.try_recv() {
            steps.push(step);
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let ticket = capture.finish().expect("the take must finish");
    let outcome = ticket.recv().expect("the take must be answered");
    let Outcome::Transcript(transcript) = outcome else {
        panic!("a spoken take must not read as silence: {outcome:?}");
    };
    assert!(!transcript.truncated, "no segment may outrun its decode budget");

    // Completeness across the join: the first paragraphs sit in an
    // early segment, the last in the tail, so both pieces reached the
    // landed text in order.
    let asr_lower = transcript.asr.to_lowercase();
    let text_lower = transcript.text.to_lowercase();
    for opener in PIPELINE_OPENERS {
        assert!(
            asr_lower.contains(opener),
            "the recognition must carry the paragraph at {opener:?} - the join lost words"
        );
        assert!(
            text_lower.contains(opener),
            "the normalized text must carry the paragraph at {opener:?}"
        );
    }

    // The tally gained its total at stop and counted everything down.
    while let Ok(step) = progress.try_recv() {
        steps.push(step);
    }
    let Some(last) = steps.last() else { panic!("the take must report its progress: {steps:?}") };
    assert_eq!(
        last.done,
        last.total.expect("the stop must make the total known"),
        "the final step is the completed take, got {steps:?}"
    );
    eprintln!("gate: {last:?}");

    // Normalize once, over the join: the diagnostics store wrote the
    // exact normalizer input, and the landed text must be one pass
    // over it - not a per-segment rewrite, which reads differently.
    let take_dir = std::fs::read_dir(diagnostics.path())
        .expect("the diagnostics store must exist")
        .find_map(|entry| Some(entry.ok()?.path()))
        .expect("one take record");
    let joined =
        std::fs::read_to_string(take_dir.join("joined.txt")).expect("the join is recorded");
    let text = std::fs::read_to_string(take_dir.join("text.txt")).expect("the text is recorded");
    let joined_lower = joined.to_lowercase();
    for opener in PIPELINE_OPENERS {
        assert!(
            joined_lower.contains(opener),
            "the recorded join must carry the paragraph at {opener:?}"
        );
    }
    let normalizer_path = dirs::cache_dir()
        .map(|d| d.join("forge-dictate").join(Config::default().normalizer.expect("shipped").file))
        .expect("cache");
    let normalizer =
        crate::normalize::Normalizer::load(&normalizer_path).expect("the normalizer must load");
    let one_pass = normalizer
        .normalize_with(&joined, Config::default().normalize_options)
        .expect("the recorded join must normalize");
    assert_eq!(text, one_pass, "the landed text must be exactly one normalizer pass over the join");
}

/// Cancelling a pipelined take mid-flight must end it: no hang in the
/// drop, the microphone and engine both still usable afterwards.
#[test]
#[ignore = "needs the ASR weights; generates a ~90 s `say` clip on first run"]
fn a_pipelined_cancel_gate_ends_cleanly() {
    shipped_weights_on_disk();
    let pcm = read_clip("cancel", PIPELINE_SCRIPT);
    let engine = crate::test_support::engine_with_feeding_microphone(
        Config::default(),
        Arc::new(pcm),
        Duration::from_millis(100),
    )
    .expect("engine must start");
    engine.wait_ready().expect("the weights must load");
    let mut capture = engine.try_capture("pipeline-cancel").expect("the mic must be held");
    let progress = capture.take_progress().expect("the capture carries a progress stream");

    let mut steps = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        while let Ok(step) = progress.try_recv() {
            steps.push(step);
        }
        if steps.iter().any(|step| step.done >= 1) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no segment settled while the recording was open: {steps:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // Abandon after the stop, while the tail is still owed: the cancel
    // must reach the outstanding jobs and the drop must join.
    let ticket = capture.finish().expect("the take must finish");
    drop(ticket);

    // The engine takes another take and answers it - a real one, so
    // the cancellation demonstrably left working weights behind.
    let second = engine.try_capture("pipeline-cancel-2").expect("the mic must be free");
    drop(second);
    let outcome = engine
        .transcribe(crate::Samples::mono(vec![0.5; crate::SAMPLE_RATE as usize]))
        .expect("queued")
        .recv()
        .expect("the engine must still answer");
    assert!(matches!(outcome, Outcome::NoAudio { .. } | Outcome::Transcript(_)));
}
