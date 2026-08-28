//! Runs the fixture corpus through the whole pipeline and writes the
//! committed results file.
//!
//! `#[ignore]`d because it needs both models on disk. `just bench` runs it.
//! It never fetches: absent weights fail with a message rather than pulling
//! three gigabytes, because a cron that quietly downloads that much is a
//! surprise nobody asked for.
//!
//! # What the file is for, and why it is shaped this way
//!
//! It exists to be diffed across commits, so the governing property is
//! that an unchanged pipeline leaves it BYTE-IDENTICAL and produces no
//! diff. Every figure is therefore held to a deadband against the value
//! already committed - see `support/results.rs` for why a deadband rather
//! than rounding.
//!
//! Consequence worth knowing: a drift smaller than the resolution never
//! updates the file. **The file is the trend, stdout is the truth** - every
//! run prints its raw unsettled numbers, so a creep is visible to whoever
//! ran it even while the file sits unchanged.
//!
//! # Numbers discipline
//!
//! Speed figures here are MEASURED. There is no accuracy score anywhere:
//! per-clip identity is the signal, a percentage would hide which clip
//! moved, and the baselines are another model's output rather than ground
//! truth. Superwhisper's figures are a quoted reference in their own block,
//! never a shared row and never a ratio - it is a macOS app no harness can
//! invoke.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

#[path = "support/results.rs"]
mod results;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use forge_dictate::{Config, Engine, ModelSpec, Outcome, SAMPLE_RATE, Samples, Stages};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use results::{Section, committed, render, settle};

/// Deadbands in milliseconds, one per committed figure.
///
/// Measured on a WORKING machine with other jobs running, which is the
/// condition this pipeline runs under. Fourteen runs on an unchanged tree,
/// each bucket at roughly 1.5x the observed spread:
///
/// ```text
/// figure        min    max  spread   bucket
/// resample       16     19       3       10
/// mel            11     14       3       10
/// encode       1494   1548      54      100
/// decode        636    820     184      300
/// normalize     957   1049      92      150
/// pipeline     3912   4226     314      500
/// ```
///
/// Spreads are WARM. The limits these buy are stated in the generated file
/// itself, where the `git diff` reader is.
const RESOLUTION_MS: &[(&str, u64)] = &[
    ("resample", 10),
    ("mel", 10),
    ("encode", 100),
    ("decode", 300),
    ("normalize", 150),
    ("pipeline", 500),
];

#[derive(Debug, Deserialize)]
struct Entry {
    file: String,
    baseline_asr: String,
    baseline_normalized: String,
}

fn resolution(key: &str) -> u64 {
    RESOLUTION_MS
        .iter()
        .find(|(k, _)| *k == key)
        .map_or_else(|| panic!("no resolution declared for {key}"), |(_, v)| *v)
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn sysctl(key: &str) -> String {
    Command::new("sysctl")
        .args(["-n", key])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or_else(|| "unknown".to_owned(), |s| s.trim().to_owned())
}

fn os_version() -> String {
    Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or_else(|| "unknown".to_owned(), |s| format!("macOS {}", s.trim()))
}

/// One results file per machine. Ved runs three Macs; a run on one must
/// not rewrite another's trend, and a different machine creating a NEW
/// untracked file is a visible addition rather than a misleading diff.
fn machine_slug(model: &str) -> String {
    model.to_lowercase().replace(|c: char| !c.is_ascii_alphanumeric(), "-")
}

fn decode_wav(path: &Path) -> Vec<f32> {
    let mut reader = hound::WavReader::open(path).expect("fixture must be readable");
    assert_eq!(reader.spec().sample_rate, SAMPLE_RATE, "fixture is not at the model's rate");
    assert_eq!(reader.spec().channels, 1, "fixture is not mono");
    reader
        .samples::<i16>()
        .map(|s| f32::from(s.expect("fixture sample must decode")) / 32768.0)
        .collect()
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Model identity for the results header: the digest `ModelSpec` declares,
/// not a fresh hash of the file.
///
/// Re-hashing the weights cost 111 s of a 115 s run and answered nothing
/// the declared digest does not: this value exists to say whether the model
/// changed, and `ModelSpec` is what changes when it does. The size check is
/// O(1) and catches the realistic corruption.
fn model_identity(spec: &ModelSpec, path: &Path) -> String {
    let len = std::fs::metadata(path).expect("model must be readable").len();
    assert_eq!(
        len, spec.size,
        "{} is {len} bytes on disk but {} declares {}. The bench refuses rather than reporting \
         timings for a model it cannot identify.",
        spec.file, spec.file, spec.size
    );
    spec.sha256[..8].to_owned()
}

/// The committed figure set, extracted so it is testable without weights.
///
/// `pipeline` is NOT fully load-free: `Stages::model_load` times the ASR
/// weights only, and the normalizer's comparable 1.5 GB loads after that
/// timer stops. How that cost distributes is unmeasured.
fn figures(end_to_end: Duration, total: &Stages) -> [(&'static str, u64); 6] {
    [
        ("resample", ms(total.resample)),
        ("mel", ms(total.mel)),
        ("encode", ms(total.encode)),
        ("decode", ms(total.decode)),
        ("normalize", ms(total.normalize.unwrap_or_default())),
        ("pipeline", ms(end_to_end.saturating_sub(total.model_load))),
    ]
}

#[test]
fn the_committed_pipeline_figure_excludes_the_asr_model_load() {
    let cold = Stages {
        model_load: Duration::from_millis(6977),
        normalize: Some(Duration::from_millis(1000)),
        ..Stages::default()
    };
    let wall = Duration::from_millis(10694);

    let figures = figures(wall, &cold);
    let pipeline = figures.iter().find(|(k, _)| *k == "pipeline").expect("declared above").1;

    assert_eq!(
        pipeline, 3717,
        "the committed pipeline figure must be the wall clock minus the reported model load. \
         Leaving the load in lets one cold run write a 10694 ms value and relocate the deadband \
         anchor permanently"
    );
    assert!(
        !figures.iter().any(|(k, _)| *k == "model_load"),
        "model_load is bimodal and must not be committed; a cold run would move the anchor and \
         every later run would be measured against the anomaly"
    );
}

#[test]
#[ignore = "needs both models on disk; run via just bench"]
fn bench_corpus() {
    let models = dirs::cache_dir()
        .map(|d| d.join("forge-dictate"))
        .expect("a cache directory is required to locate the weights");
    let asr = ModelSpec::cohere_transcribe_q4_k_m();
    let normalizer = ModelSpec::s1_mini_f16();

    for spec in [&asr, &normalizer] {
        let path = models.join(&spec.file);
        assert!(
            path.exists(),
            "{} is not on disk at {}. This bench does not fetch - run prepare() first. \
             Downloading three gigabytes as a side effect of a benchmark, or of a cron, is a \
             surprise nobody asked for.",
            spec.file,
            path.display()
        );
    }

    let raw = std::fs::read_to_string(fixtures_dir().join("manifest.json")).unwrap();
    let manifest: Vec<Entry> = serde_json::from_str(&raw).unwrap();
    let manifest_sha = hex::encode(Sha256::digest(raw.as_bytes()))[..8].to_owned();

    let construct_start = std::time::Instant::now();
    let engine = Engine::new(Config::default()).expect("engine must start");
    let engine_construct = construct_start.elapsed();
    let mut first_clip = Duration::ZERO;
    let mut asr_differs = Vec::new();

    let mut total = Stages {
        model_load: Duration::ZERO,
        resample: Duration::ZERO,
        mel: Duration::ZERO,
        encode: Duration::ZERO,
        decode: Duration::ZERO,
        normalize: Some(Duration::ZERO),
        audio: Duration::ZERO,
    };
    let mut end_to_end = Duration::ZERO;
    let mut matches = 0usize;
    let mut differs = Vec::new();

    // No warm-up pass: measured, it widened the spread rather than
    // tightening it. The variation is between processes, not within one.

    for (index, entry) in manifest.iter().enumerate() {
        let samples = decode_wav(&fixtures_dir().join(&entry.file));
        let started = std::time::Instant::now();
        let outcome = engine
            .transcribe(Samples::mono(samples))
            .expect("fixture is mono at the model's rate")
            .recv()
            .expect("transcription must not fail");
        let elapsed = started.elapsed();
        end_to_end += elapsed;
        if index == 0 {
            first_clip = elapsed;
        }

        let Outcome::Transcript(t) = outcome else {
            panic!("{} produced no audio; the corpus is speech", entry.file);
        };

        total.model_load += t.stages.model_load;
        total.resample += t.stages.resample;
        total.mel += t.stages.mel;
        total.encode += t.stages.encode;
        total.decode += t.stages.decode;
        total.audio += t.stages.audio;
        if let (Some(acc), Some(one)) = (total.normalize.as_mut(), t.stages.normalize) {
            *acc += one;
        }

        // Split the two stages. A whole-pipeline difference says nothing
        // about WHERE it arose, and the corpus is a drift detector for
        // both halves separately.
        if t.asr != entry.baseline_asr {
            asr_differs.push(entry.file.clone());
        }
        if t.text == entry.baseline_normalized {
            matches += 1;
        } else {
            differs.push(entry.file.clone());
        }
    }

    // Raw and unsettled, every run. The committed file may not move; this
    // always does, and it is what makes a sub-resolution creep visible.
    println!("\n=== RAW, unsettled (the file is the trend, this is the truth) ===");
    println!("model_load {:>7} ms", ms(total.model_load));
    println!("resample   {:>7} ms", ms(total.resample));
    println!("mel        {:>7} ms", ms(total.mel));
    println!("encode     {:>7} ms", ms(total.encode));
    println!("decode     {:>7} ms", ms(total.decode));
    println!("normalize  {:>7} ms", ms(total.normalize.unwrap_or_default()));
    println!("wall clock {:>7} ms  (committed `pipeline` subtracts model_load)", ms(end_to_end));
    println!("audio      {:>7} ms", ms(total.audio));
    println!(
        "realtime   {:>7.1}x",
        total.audio.as_secs_f64() / end_to_end.as_secs_f64().max(f64::MIN_POSITIVE)
    );
    println!("\n--- where the wall clock goes, which is NOT the pipeline ---");
    println!("engine_construct {:>7} ms", ms(engine_construct));
    println!("first clip       {:>7} ms  (carries lazy model load)", ms(first_clip));
    println!("other 14 clips   {:>7} ms", ms(end_to_end.saturating_sub(first_clip)));
    println!(
        "\n--- accuracy, per stage, against the locked baselines ---\n\
         asr differs from baseline_asr:             {:>2} of {}  {:?}\n\
         pipeline differs from baseline_normalized: {:>2} of {}  {:?}  ({matches} matched)",
        asr_differs.len(),
        manifest.len(),
        asr_differs,
        differs.len(),
        manifest.len(),
        differs
    );

    let model = sysctl("hw.model");
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("bench")
        .join(format!("{}.toml", machine_slug(&model)));
    let existing = std::fs::read_to_string(&path).ok();

    // `model_load` is deliberately NOT committed. It is bimodal - roughly
    // 450 ms with the weights in page cache and 6977 ms cold - and the
    // scheduled run guarantees cold, so no single bucket works: one wide
    // enough to span both regimes could only catch a regression larger
    // than the cold value itself, and a tight one produces a bogus diff on
    // every scheduled run. `Stages::model_load` says as much one file over
    // ("a cold start is not a regression"). It still prints on every run,
    // which is the file-is-the-trend / stdout-is-the-truth split doing its
    // job.
    let sections: Vec<Section> = figures(end_to_end, &total)
        .iter()
        .map(|(key, measured)| {
            let name = format!("speed.{key}");
            let prior = committed(existing.as_deref(), &name, "ms")
                .expect("an unreadable results file must not be silently overwritten");
            Section {
                name,
                key: "ms",
                value: settle(*measured, prior, resolution(key)),
                resolution: resolution(key),
            }
        })
        .collect();

    let header_start = std::time::Instant::now();
    let mut out = String::new();
    out.push_str(&header(
        &model,
        &manifest_sha,
        &manifest,
        total.audio,
        &asr,
        &normalizer,
        &models,
    ));
    println!("header {:>7} ms", ms(header_start.elapsed()));
    out.push_str(&render(&sections));
    out.push_str(&accuracy(&manifest, &differs, &asr_differs));
    out.push_str(REFERENCE);

    std::fs::create_dir_all(path.parent().expect("has a parent")).expect("bench dir");
    std::fs::write(&path, out).expect("results file must be writable");
    println!("\nwrote {}", path.display());
}

fn header(
    model: &str,
    manifest_sha: &str,
    manifest: &[Entry],
    audio: Duration,
    asr: &ModelSpec,
    normalizer: &ModelSpec,
    models: &Path,
) -> String {
    format!(
        "# forge-dictate bench. Regenerated by `just bench`. Commit the result.\n\
         # Do not hand-edit: an unreadable file makes the next run refuse rather\n\
         # than silently start the trend over.\n\
         #\n\
         # Measured on a WORKING machine, not a quiesced one - other jobs were\n\
         # running. That is the condition this pipeline runs under, so a\n\
         # comparison against a quiet-machine run is not like for like.\n\
         #\n\
         # The model sha256s sit beside the timings on purpose: if they are\n\
         # unchanged and a stage moved, it was our code.\n\n\
         [machine]\n\
         model = \"{model}\"\n\
         cpu = \"{}\"\n\
         os = \"{}\"\n\n\
         [corpus]\n\
         clips = {}\n\
         audio_seconds = {}\n\
         manifest_sha256 = \"{manifest_sha}\"\n\n\
         [asr]\n\
         model = \"{}\"\n\
         sha256 = \"{}\"\n\n\
         [normalizer]\n\
         model = \"{}\"\n\
         sha256 = \"{}\"\n\n\
         # Speed. MEASURED and reproducible. Each figure is held to its\n\
         # resolution in ms: a new measurement within that many of the committed\n\
         # value leaves this file untouched, so a diff here is a real move.\n\
         #\n\
         # What that costs, so nobody reads more into a green file than is there:\n\
         #\n\
         #   decode is the noisiest stage (636-820 ms across runs), so its 300 ms\n\
         #   bucket hides anything short of a ~44% regression. That is the\n\
         #   measurement's limit, not the bucket's - a tighter one would churn\n\
         #   rather than resolve.\n\
         #\n\
         #   resample and mel have 10 ms buckets against 16-19 ms and 11-14 ms\n\
         #   values. So mel can reach 22 ms before this file moves. Their buckets\n\
         #   exceed their SPREADS by 3.3x where every other figure sits at\n\
         #   1.6-1.9x, which is affordable only because the two together are\n\
         #   under 1% of a run.\n\
         #\n\
         #   model_load is NOT here. It is bimodal - about 450 ms warm and 6977 ms\n\
         #   cold - so no bucket both survives a scheduled run and detects\n\
         #   anything. pipeline below subtracts it.\n\
         #\n\
         #   pipeline is NOT a fully load-free figure. The engine times model_load\n\
         #   around the ASR weights only; the normalizer's comparable 1.5 GB loads\n\
         #   after that timer stops and is still inside this number. How that cost\n\
         #   distributes is unmeasured, so do not read pipeline as pure compute.\n\n",
        sysctl("machdep.cpu.brand_string"),
        os_version(),
        manifest.len(),
        audio.as_secs(),
        asr.file,
        model_identity(asr, &models.join(&asr.file)),
        normalizer.file,
        model_identity(normalizer, &models.join(&normalizer.file)),
    )
}

fn accuracy(manifest: &[Entry], differs: &[String], asr_differs: &[String]) -> String {
    let mut out = String::from(
        "# Accuracy. DIRECTIONAL, never a score.\n\
         #\n\
         # Per-clip identity is the signal: a percentage would hide which clip\n\
         # moved, and the baselines are another model's locked output rather\n\
         # than ground truth, so a difference is a question and not a verdict.\n\
         # Divergence is expected - see fixtures/README.md. Nothing here may be\n\
         # tuned toward these baselines.\n\
         #\n\
         # This is the WHOLE pipeline against baseline_normalized. The\n\
         # normalizer-only comparison, with both texts printed verbatim, is\n\
         # `cargo nextest run -p forge-dictate --test normalizer_baselines\n\
         # --run-ignored all --no-capture`.\n\n\
         [accuracy]\n",
    );
    for entry in manifest {
        let status = if differs.contains(&entry.file) { "differs" } else { "matches" };
        let _ = writeln!(out, "\"{}\" = \"{status}\"", entry.file);
    }

    out.push_str(
        "\n# The ASR half, against baseline_asr. Committed separately because a\n\
         # whole-pipeline result cannot say WHERE a difference arose, and an ASR\n\
         # change the normalizer happens to absorb would otherwise produce no\n\
         # diff at all.\n\n\
         [accuracy_asr]\n",
    );
    for entry in manifest {
        let status = if asr_differs.contains(&entry.file) { "differs" } else { "matches" };
        let _ = writeln!(out, "\"{}\" = \"{status}\"", entry.file);
    }
    out.push('\n');
    out
}

/// Quoted, never reproduced. Superwhisper is a macOS app no harness can
/// invoke, so this block is recorded rather than measured here, sits apart
/// from our figures, and carries no ratio against them.
const REFERENCE: &str = "\
# Recorded reference, NOT run by this bench and not like for like.
#
# Source: .plans/forge-dictate-implementation.md line 49, which records
# Superwhisper's MLX at 63.2x realtime. That note does not say who measured
# it, on what machine, or against what corpus, so the citation stops at
# \"the research notes record it as measured\".
#
# Deliberately not placed beside our figures and deliberately without a
# ratio: two numbers in adjacent rows imply a like-for-like run that never
# happened.

[reference]
superwhisper_realtime_factor = 63.2
";
