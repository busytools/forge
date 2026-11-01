//! Runs the fixture corpus through the whole pipeline and writes the
//! committed results file.
//!
//! `#[ignore]`d because it needs both models on disk. `just bench` runs it.
//! It never fetches: absent weights fail with a message rather than pulling
//! three gigabytes, because a cron that quietly downloads that much is a
//! surprise nobody asked for.
//!
//! **It measures the models the live roles resolve to on this machine**, not
//! a hardcoded pin. A trend is only a signal about what a user gets if the
//! model beside the timings is the one dictate actually runs, and a
//! resolution this process cannot read is a failure rather than a quiet
//! fall back to a pin.
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

use forge_dictate::{Engine, ModelSpec, Normalizer, Outcome, SAMPLE_RATE, Samples, Stages};
use forge_workspace::DictateRole;
use forge_workspace::bench::{LiveRoles, resolved_live_roles};
use forge_workspace::install::{ActiveChoice, ActiveFrom, InstalledModel};
use forge_workspace::store::{self, Db, dictate_models};
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

/// The fixture corpus stays with the dictate crate, whose own tests read it
/// too; this bench is the second reader.
fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../forge-dictate/fixtures")
}

/// `$CLAUDE_CONFIG_DIR`, else `~/.claude`: the config dir a running forge
/// boots from, which is where the roles' resolution lives.
fn config_dir() -> PathBuf {
    if let Ok(value) = std::env::var("CLAUDE_CONFIG_DIR") {
        let trimmed = value.trim_end_matches('/');
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    dirs::home_dir().expect("a home directory is required to locate the config dir").join(".claude")
}

/// The models the live roles resolve to, for a process that is not forge.
///
/// A failure here is the bench refusing to run: falling back to the compiled
/// pins is how the trend came to measure a model nobody was running.
fn live_roles() -> LiveRoles {
    let app_support =
        forge_sdk::app_support_dir().expect("a data directory is required to locate forge's store");
    resolved_live_roles(&config_dir(), &app_support).unwrap_or_else(|error| {
        panic!(
            "the bench measures the model the live dictate role resolves to, and that \
             resolution failed: {error}"
        )
    })
}

/// Where one role's model came from, in the words a stdout reader needs.
fn source_of(live: &LiveRoles, role: DictateRole) -> String {
    match live.models.iter().find(|(runner, _)| *runner == role).map(|(_, model)| &model.from) {
        Some(ActiveFrom::Config { key, variant }) => {
            format!("pinned by [dictate] {key} = \"{variant}\"")
        }
        Some(ActiveFrom::Installed { variant }) => format!("the runtime pick, \"{variant}\""),
        Some(ActiveFrom::Pin) => "the compiled pin".to_owned(),
        None => "no role".to_owned(),
    }
}

/// The string the committed trend holds at `section.key`, if any.
fn committed_str(file: Option<&str>, section: &str, key: &str) -> Result<Option<String>, String> {
    let Some(value) = results::lookup(file, section, key)? else {
        return Ok(None);
    };
    value
        .as_str()
        .map(str::to_owned)
        .map(Some)
        .ok_or_else(|| format!("{section}.{key} is present but is not a string: {value}"))
}

/// Whether the committed trend recorded the same run this one is: the same
/// machine, corpus and model identities.
///
/// Any change re-anchors every figure: keeping a deadband anchor recorded for
/// another model, corpus or machine would attribute that run's numbers to
/// this one, and the anchor would then never self-correct. The identity is
/// what the header writes, not the source - a pick becoming a config key that
/// names the same installed record keeps the anchor, while a compiled-pin
/// takeover changes the identity (a digest rather than a size).
fn same_run(
    existing: Option<&str>,
    machine: (&str, &str, &str),
    corpus_sha: &str,
    asr: (&str, &str),
    normalizer: (&str, &str),
) -> Result<bool, String> {
    let fields = [
        ("machine", "model", machine.0),
        ("machine", "cpu", machine.1),
        ("machine", "os", machine.2),
        ("corpus", "manifest_sha256", corpus_sha),
        ("asr", "model", asr.0),
        ("asr", "sha256", asr.1),
        ("normalizer", "model", normalizer.0),
        ("normalizer", "sha256", normalizer.1),
    ];
    for (section, key, now) in fields {
        let Some(prior) = committed_str(existing, section, key)? else {
            return Ok(false);
        };
        if prior != now {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The anchor a figure is held to: none when the run changed, so every
/// figure re-anchors; otherwise the committed one.
fn prior_for(prior: Option<u64>, same_model: bool) -> Option<u64> {
    if same_model { prior } else { None }
}

/// The live engine configuration with per-take diagnostics off.
///
/// **A bench run is not the user's dictation.** Its clips would otherwise
/// land in the per-take diagnostics store, the shelf the in-app model bench
/// scores; this trend bench writes no takes.
fn bench_config(live: &LiveRoles) -> forge_dictate::Config {
    let mut config = live.config.clone();
    config.diagnostics_dir = None;
    config.models_dir = Some(live.models_dir.clone());
    config
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
    // The pins all carry a digest; a spec that does not names itself by its
    // size instead, which is the one identity it declares.
    spec.sha256.as_deref().map_or_else(|| format!("size-{len}"), |digest| digest[..8].to_owned())
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

/// A machine laid out the way forge leaves it: a config dir carrying
/// `forge.toml`, and the app-support store holding what has been installed
/// and picked, so the resolution runs without the live ones.
struct Machine {
    _root: tempfile::TempDir,
    /// Held open for the lifetime of the fixture: the store is locked exactly
    /// as a running forge holds it, which is the condition the copy exists for.
    _db: Db,
    config_dir: PathBuf,
    app_support: PathBuf,
    models_dir: PathBuf,
}

impl Machine {
    /// `dictate` is the rest of the `[dictate]` section; `installed` is one
    /// record the store carries and `pick` the runtime pick for the
    /// transcribing role.
    fn with_store(dictate: &str, installed: Option<(&str, &str, u64)>, pick: Option<&str>) -> Self {
        Self::with_dictate_enabled(true, dictate, installed, pick)
    }

    fn with_dictate_enabled(
        enabled: bool,
        dictate: &str,
        installed: Option<(&str, &str, u64)>,
        pick: Option<&str>,
    ) -> Self {
        let root = tempfile::tempdir().expect("a temp dir");
        let config_dir = root.path().join("claude");
        std::fs::create_dir_all(config_dir.join("forge")).expect("the forge dir");
        let models_dir = root.path().join("models");
        std::fs::create_dir_all(&models_dir).expect("the models dir");
        std::fs::write(
            config_dir.join("forge/forge.toml"),
            format!(
                "[[orgs]]\nname = \"Personal\"\naccounts = [\"Stargate\"]\n\n\
                 [[orgs.projects]]\nname = \"forge\"\npath = \"~/Projects/forge\"\n\n\
                 [[accounts]]\ndisplay_name = \"Stargate\"\ntoken = \"t\"\n\
                 models = [\"claude-sonnet-5\"]\nprovider = \"anthropic\"\n\n\
                 [dictate]\nenabled = {enabled}\nmodels_dir = \"{}\"\n{dictate}",
                models_dir.display()
            ),
        )
        .expect("forge.toml must be writable");
        let app_support = root.path().join("support");
        std::fs::create_dir_all(app_support.join("stores")).expect("the store dir");

        let db = Db::open(&store::path_for(&app_support, &config_dir)).expect("the store opens");
        if let Some((variant, file, size)) = installed {
            dictate_models::record_installed(
                &db,
                &InstalledModel {
                    variant: variant.to_owned(),
                    file: file.to_owned(),
                    url: format!("https://weights.invalid/{file}"),
                    size,
                    facts: forge_dictate::ModelFacts::default(),
                    at: "2026-11-01T00:00:00Z".to_owned(),
                },
            )
            .expect("the record lands");
        }
        if let Some(variant) = pick {
            dictate_models::record_active(
                &db,
                DictateRole::Transcribing,
                &ActiveChoice {
                    variant: variant.to_owned(),
                    at: "2026-11-01T00:00:00Z".to_owned(),
                },
            )
            .expect("the pick lands");
        }

        Self { _root: root, _db: db, config_dir, app_support, models_dir }
    }

    fn resolve(&self) -> Result<LiveRoles, String> {
        resolved_live_roles(&self.config_dir, &self.app_support)
    }
}

/// **The bench measures the model the live role resolves to, not the
/// compiled pin.** Measuring the pin while the user runs an installed pick is
/// the defect this reads for: every committed trend named a model nobody was
/// running, so a quality gap between the two was invisible.
#[test]
fn the_bench_takes_the_transcribing_model_from_the_live_pick() {
    let granite = "granite-speech-5.0-470m-turboctc-nc-Q4_K_M.gguf";
    let machine = Machine::with_store("", Some(("granite", granite, 279_393_440)), Some("granite"));

    let live = machine.resolve().expect("the live roles resolve");

    assert_eq!(live.config.asr_model.file, granite, "the transcribing role runs the pick");
    assert_eq!(live.config.asr_model.size, 279_393_440, "with the size its record carries");
    assert_eq!(
        live.models_dir, machine.models_dir,
        "the models dir is [dictate] models_dir, not a cache default"
    );
    assert!(
        live.models.iter().any(|(role, model)| *role == DictateRole::Transcribing
            && matches!(&model.from, ActiveFrom::Installed { .. })),
        "and the resolution carries where the model came from, which the bench prints"
    );
    assert_eq!(
        live.config.normalizer.map(|spec| spec.file),
        Some(ModelSpec::s1_mini_f16().file),
        "the cleanup role has no key and no pick, so it falls to the compiled pin"
    );
}

/// **A config key decides before the pick is read.** The bench resolves a
/// keyed role from the key, as the boot does; the pick row must not change
/// the answer.
#[test]
fn a_config_key_beats_the_pick_in_the_bench_resolution() {
    let keyed = "keyed-speech.gguf";
    let machine = Machine::with_store(
        "transcribe_model = \"keyed\"\n",
        Some(("keyed", keyed, 100)),
        Some("picked"),
    );

    let live = machine.resolve().expect("the key resolves");

    assert_eq!(live.config.asr_model.file, keyed, "the key's model runs, not the pick");
    assert!(
        live.models.iter().any(|(role, model)| *role == DictateRole::Transcribing
            && matches!(&model.from, ActiveFrom::Config { .. })),
        "and the resolution names the key as the source",
    );
}

/// **A store the bench cannot read is a failure, not a fall back to the
/// pins.** The fallback is the defect restated: it measures a model the user
/// is not running, and says nothing about it.
#[test]
fn a_machine_with_no_store_fails_rather_than_measuring_the_pin() {
    let machine = Machine::with_store("", None, None);
    let store = store::path_for(&machine.app_support, &machine.config_dir);
    std::fs::remove_file(&store).expect("the store was written");

    let error = machine.resolve().expect_err("no store, no resolution");

    assert!(error.contains("could not be read"), "got: {error}");
    assert!(
        error.contains(&store.display().to_string()),
        "the error names the store it wanted, got: {error}"
    );
}

/// **A machine with dictation off has no live role to measure.** The bench
/// refuses by name rather than resolving pins for a role a running forge
/// never starts.
#[test]
fn a_machine_with_dictation_off_fails_by_name() {
    let machine = Machine::with_dictate_enabled(false, "", None, None);
    // No store either: the gate must speak before anything is read, so a
    // store error here would mean the check moved below the read.
    std::fs::remove_file(store::path_for(&machine.app_support, &machine.config_dir))
        .expect("the store was written");

    let error = machine.resolve().expect_err("no live role when dictation is off");

    assert!(error.contains("[dictate] enabled = false"), "got: {error}");
}

/// **A changed run re-anchors the trend.** The header names this run's
/// machine, corpus and models while the figures are held to the file's
/// deadband, so an anchor recorded for another run would attribute its
/// numbers to this one and never self-correct.
#[test]
fn a_changed_run_re_anchors_the_trend() {
    let committed = "[machine]\nmodel = \"Mac16,9\"\ncpu = \"M4\"\nos = \"macOS\"\n\n\
                     [corpus]\nmanifest_sha256 = \"corpus1\"\n\n\
                     [asr]\nmodel = \"a.gguf\"\nsha256 = \"sha256:aa\"\n\n\
                     [normalizer]\nmodel = \"n.gguf\"\nsha256 = \"size-10\"\n";
    let machine = ("Mac16,9", "M4", "macOS");
    let asr = ("a.gguf", "sha256:aa");
    let normalizer = ("n.gguf", "size-10");
    let same =
        |machine: (&str, &str, &str), corpus: &str, asr: (&str, &str), norm: (&str, &str)| {
            same_run(Some(committed), machine, corpus, asr, norm).expect("a valid file")
        };
    assert!(same(machine, "corpus1", asr, normalizer), "the same run keeps its anchors");
    assert!(
        !same(machine, "corpus1", ("b.gguf", "sha256:aa"), normalizer),
        "a different asr file re-anchors",
    );
    assert!(
        !same(machine, "corpus1", ("a.gguf", "size-279"), normalizer),
        "the same asr file with a new identity re-anchors",
    );
    assert!(
        !same(machine, "corpus1", asr, ("m.gguf", "size-10")),
        "a different normalizer file re-anchors",
    );
    assert!(
        !same(machine, "corpus1", asr, ("n.gguf", "size-99")),
        "a normalizer identity change re-anchors",
    );
    assert!(!same(machine, "corpus2", asr, normalizer), "a different corpus re-anchors");
    assert!(
        !same(("Mac16,9", "M3", "macOS"), "corpus1", asr, normalizer),
        "a different machine re-anchors",
    );
    assert!(
        !same_run(None, machine, "corpus1", asr, normalizer).expect("no file"),
        "a first run re-anchors",
    );
    assert!(
        !same_run(Some("[machine]\nmodel = \"Mac\"\n"), machine, "corpus1", asr, normalizer)
            .expect("a file without the sections"),
        "a file without the sections is a first run",
    );
    assert_eq!(
        prior_for(Some(682), true),
        Some(682),
        "an unchanged run keeps the committed anchor",
    );
    assert_eq!(prior_for(Some(682), false), None, "a changed run re-anchors every figure");
}

/// **A config key the machine has not installed fails, rather than reaching
/// the feed.** A bench fetches nothing, and dropping to the pin here would
/// measure a model nobody chose.
#[test]
fn a_config_key_with_no_installed_record_fails_rather_than_fetching() {
    let machine = Machine::with_store("transcribe_model = \"lfm3\"\n", None, None);

    let error = machine.resolve().expect_err("nothing resolves that key");

    assert!(
        error.contains("[dictate] transcribe_model names lfm3")
            && error.contains("has not installed"),
        "the error names the key, the variant and why, got: {error}"
    );
}

#[test]
#[ignore = "needs both models on disk; run via just bench"]
fn bench_corpus() {
    let live = live_roles();
    let models = live.models_dir.clone();
    let asr = live.config.asr_model.clone();
    let normalizer = live.config.normalizer.clone().expect(
        "the normalization role is off in this forge.toml ([dictate] normalizer = false); the \
         trend measures both roles",
    );

    for (role, spec) in
        [(DictateRole::Transcribing, &asr), (DictateRole::Normalization, &normalizer)]
    {
        let path = models.join(&spec.file);
        assert!(
            path.exists(),
            "{} is not on disk at {}. The {} resolves to it ({}), so the bench measures that \
             model or nothing. This bench does not fetch - run prepare() first. Downloading three \
             gigabytes as a side effect of a benchmark, or of a cron, is a surprise nobody asked \
             for.",
            spec.file,
            path.display(),
            role.label(),
            source_of(&live, role)
        );
    }

    let raw = std::fs::read_to_string(fixtures_dir().join("manifest.json")).unwrap();
    let manifest: Vec<Entry> = serde_json::from_str(&raw).unwrap();
    let manifest_sha = hex::encode(Sha256::digest(raw.as_bytes()))[..8].to_owned();

    let construct_start = std::time::Instant::now();
    let engine = Engine::new(bench_config(&live)).expect("engine must start");
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
    println!("asr         {}  ({})", asr.file, source_of(&live, DictateRole::Transcribing));
    println!("normalizer  {}  ({})", normalizer.file, source_of(&live, DictateRole::Normalization));
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
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!(
            "the results file {} could not be read ({error}); refusing to overwrite a trend it \
             cannot read",
            path.display()
        ),
    };
    let asr_identity = model_identity(&asr, &models.join(&asr.file));
    let normalizer_identity = model_identity(&normalizer, &models.join(&normalizer.file));
    let cpu = sysctl("machdep.cpu.brand_string");
    let os = os_version();
    let same_model = same_run(
        existing.as_deref(),
        (&model, &cpu, &os),
        &manifest_sha,
        (&asr.file, &asr_identity),
        (&normalizer.file, &normalizer_identity),
    )
    .expect("an unreadable results file must not be silently overwritten");

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
            let prior = prior_for(prior, same_model);
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

/// The two model loads, timed separately, each twice.
///
/// `Stages::model_load` times the ASR weights only, and the normalizer's
/// comparable 1.5 GB loads after that timer stops and lands inside
/// `pipeline` - the gap the results file's own notes call out as
/// unmeasured. This times both through the calls the engine makes, and
/// loads each a second time in the same process so a cold pass and a warm
/// one are separable.
///
/// Nothing here reaches the results file: the load is bimodal for the same
/// reason the file gives for leaving `model_load` out of itself, so its
/// figures are stdout only, like that one.
///
/// COLD MEANS THE PAGE CACHE HOLDS NONE OF THE WEIGHTS, and two things take
/// that away: a machine that has just read them, and a running forge, which
/// holds the normalizer mapped so the purge cannot evict it. Run this after
/// `sudo purge` - bare `purge` refuses without root - and point
/// `FORGE_DICTATE_BENCH_MODELS` at a copy of the weights when something live
/// has the originals mapped: the copy is the same bytes with no mapping, so
/// the purge reaches it, and `model_identity` checks it against the spec.
/// The override is this test's alone; `bench_corpus` always reads the models
/// directory the live roles resolve. A run without either is warm throughout
/// and is the control.
///
/// Run it as `just bench` never does, in a process of its own:
///
/// ```text
/// cargo nextest run -p forge-workspace --test bench --run-ignored all \
///   --no-capture -E 'test(model_load_cold_warm)'
/// ```
#[test]
#[ignore = "needs both models on disk; measures model load only"]
fn model_load_cold_warm() {
    let live = live_roles();
    let models = std::env::var_os("FORGE_DICTATE_BENCH_MODELS")
        .map_or_else(|| live.models_dir.clone(), PathBuf::from);
    let asr = live.config.asr_model.clone();
    let normalizer = live.config.normalizer.clone().expect(
        "the normalization role is off in this forge.toml ([dictate] normalizer = false); this \
         measurement loads both roles' weights",
    );
    let asr_path = models.join(&asr.file);
    let normalizer_path = models.join(&normalizer.file);
    let _ = model_identity(&asr, &asr_path);
    let _ = model_identity(&normalizer, &normalizer_path);

    let load_asr = || {
        let started = std::time::Instant::now();
        let model = transcribe_cpp::Model::load(&asr_path).expect("asr weights must load");
        let session = model.session().expect("a session must open over the weights");
        let elapsed = started.elapsed();
        drop(session);
        drop(model);
        elapsed
    };
    let load_normalizer = || {
        let started = std::time::Instant::now();
        let normalizer = Normalizer::load(&normalizer_path).expect("normalizer weights must load");
        let elapsed = started.elapsed();
        drop(normalizer);
        elapsed
    };

    println!("\n=== model loads, separately (nothing here reaches the results file) ===");
    for pass in ["first (cold)", "second (warm, same process)"] {
        let asr_ms = ms(load_asr());
        let normalizer_ms = ms(load_normalizer());
        println!(
            "{pass:<32} asr {asr_ms:>6} ms, normalizer {normalizer_ms:>6} ms, both {:>6} ms",
            asr_ms + normalizer_ms
        );
    }

    // The app-visible path: the engine loads both on its worker thread, and
    // `wait_ready` resolves once the second is in memory.
    let started = std::time::Instant::now();
    let mut engine_config = bench_config(&live);
    engine_config.models_dir = Some(models.clone());
    let engine = Engine::new(engine_config).expect("engine must start");
    engine.wait_ready().expect("both models must load");
    println!("\nengine (new + wait_ready): {} ms", ms(started.elapsed()));
    drop(engine);
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
        "# The dictate trend bench. Regenerated by `just bench`. Commit the result.\n\
         # Do not hand-edit: an unreadable file makes the next run refuse rather\n\
         # than silently start the trend over.\n\
         #\n\
         # Measured on a WORKING machine, not a quiesced one - other jobs were\n\
         # running. That is the condition this pipeline runs under, so a\n\
         # comparison against a quiet-machine run is not like for like.\n\
         #\n\
         # The model identities sit beside the timings on purpose (the digest,\n\
         # or the size where the record carries none): if they are unchanged\n\
         # and a stage moved, it was our code; if they moved, the figures\n\
         # below were re-anchored rather than kept.\n\n\
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
         # Divergence is expected - see crates/forge-dictate/fixtures/README.md.\n\
         # Nothing here may be\n\
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
