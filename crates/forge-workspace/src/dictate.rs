//! Dictation - the `[dictate]` section in `forge.toml`, and the
//! preflight pass that makes the models usable before forge starts.
//!
//! The costs are what shape this. A first run fetches 3.07 GB; every
//! later one re-hashes it, which is about 5 s for the pair; loading the
//! weights is another second warm. None of that can happen while
//! somebody is dictating, so all of it happens once, at boot, on the
//! preflight screen.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use serde::Deserialize;

/// Cap on one recording when `[dictate] max_capture_minutes` is absent.
/// Matches the crate's own default; a capture reserves 4 bytes a sample
/// eagerly, so this is about 110 MiB held for the run.
const DEFAULT_MAX_CAPTURE_MINUTES: u64 = 30;

/// The `[dictate]` section. Every field has a default, so an absent
/// section is exactly `enabled = false`.
///
/// Deliberately narrower than [`forge_dictate::Config`]. A model spec
/// carries a URL, a byte length and a digest, and hand-editing one is
/// how a file arrives that nothing can verify. The normalizer's three
/// prompt axes are permissions rather than instructions - they say what
/// the model MAY change, not what it must - so exposing them means
/// flipping one, seeing no difference, and concluding it is broken.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DictateSettings {
    /// Off unless asked for: turning it on costs a 3.07 GB download the
    /// first time and holds about 1.8 GB of physical footprint for the
    /// run.
    #[serde(default)]
    pub enabled: bool,
    /// Where the model files live. Absent uses the platform cache
    /// directory.
    #[serde(default)]
    pub models_dir: Option<String>,
    /// Input to record from, by device id rather than name: the id is
    /// what survives a restart and a rename. Absent means the system
    /// default.
    #[serde(default)]
    pub device: Option<String>,
    /// Spoken language hint. Absent autodetects.
    #[serde(default)]
    pub language: Option<String>,
    /// Rewrite recognition output into clean text. Off halves the
    /// download and skips a pass per utterance.
    #[serde(default = "enabled_by_default")]
    pub normalizer: bool,
    /// Upper bound on a single recording. A capture nobody stops ends
    /// itself here rather than holding the microphone indefinitely.
    #[serde(default = "default_max_capture_minutes")]
    pub max_capture_minutes: u64,
}

fn enabled_by_default() -> bool {
    true
}

fn default_max_capture_minutes() -> u64 {
    DEFAULT_MAX_CAPTURE_MINUTES
}

impl Default for DictateSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            models_dir: None,
            device: None,
            language: None,
            normalizer: true,
            max_capture_minutes: DEFAULT_MAX_CAPTURE_MINUTES,
        }
    }
}

impl DictateSettings {
    /// The engine configuration these settings describe.
    fn to_config(&self) -> forge_dictate::Config {
        let mut builder = forge_dictate::ConfigBuilder::new()
            .max_capture(Duration::from_secs(self.max_capture_minutes.saturating_mul(60)));
        if let Some(dir) = self.models_dir.as_deref() {
            builder = builder.models_dir(crate::config::expand_home(dir));
        }
        if let Some(device) = self.device.as_deref() {
            builder = builder.device(device);
        }
        if let Some(language) = self.language.as_deref() {
            builder = builder.language(language);
        }
        if !self.normalizer {
            builder = builder.normalizer(None);
        }
        builder.build()
    }

    /// Where the model files land. Preflight says it while fetching,
    /// because cancelling keeps what has arrived and a reader needs to
    /// know where.
    pub(crate) fn models_dir(&self) -> Option<PathBuf> {
        match self.models_dir.as_deref() {
            Some(dir) => Some(crate::config::expand_home(dir)),
            None => dirs::cache_dir().map(|dir| dir.join("forge-dictate")),
        }
    }

    /// The rows preflight draws before any work has started.
    fn initial_models(&self) -> Vec<DictateModel> {
        let cfg = self.to_config();
        std::iter::once((DictateRole::Transcribing, &cfg.asr_model))
            .chain(cfg.normalizer.as_ref().map(|spec| (DictateRole::Normalization, spec)))
            .map(|(role, spec)| DictateModel {
                role,
                file: spec.file.clone(),
                state: DictateModelState::Pending,
            })
            .collect()
    }
}

/// What a model is for. Preflight labels the row by role and prints the
/// file underneath, because the role is what a reader is scanning for
/// and the file is the detail they want only when something is wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictateRole {
    Transcribing,
    Normalization,
}

impl DictateRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::Transcribing => "transcribing model",
            Self::Normalization => "normalization model",
        }
    }
}

/// How far one model has got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DictateModelState {
    /// Nothing has started on this one.
    Pending,
    /// Bytes are moving. `resumed_from` is what a `.part` already held,
    /// so a bar that opens at 38% can say why.
    Downloading { downloaded: u64, total: u64, resumed_from: Option<u64> },
    /// Hashing what is on disk, which reads the file end to end.
    Verifying,
    /// On disk and verified, weights not loaded yet.
    Fetched,
    /// Weights going into memory.
    Loading,
    /// Loaded and usable.
    Ready,
    /// The one that stopped preflight. Which way is in
    /// [`DictateSnapshot::failure`].
    Failed,
}

/// Why preflight stopped.
///
/// Every variant ends the run: there is no degraded dictation to fall
/// back to, so the screen names the way out and forge quits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DictateFailure {
    /// A file is the right length and the wrong bytes. Reported rather
    /// than repaired - discarding a multi-gigabyte file somebody put
    /// there is not forge's call, and `size` is what makes that
    /// sentence concrete rather than abstract on screen.
    HashMismatch { path: PathBuf, expected: String, actual: String, size: u64 },
    /// The user asked preflight to stop. `kept` of `total` bytes are on
    /// disk for the file that was in flight, which the screen says
    /// before forge goes. `total` is carried so the bar can stay a
    /// fraction: a full bar beside `cancelled` reads as finished.
    Cancelled { kept: u64, total: u64 },
    /// Everything else, worded as the crate worded it.
    Other { message: String },
}

impl DictateFailure {
    /// `true` when the user stopped preflight rather than something
    /// going wrong. The screen wording and forge's exit differ.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled { .. })
    }
}

/// One model's row in preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictateModel {
    pub role: DictateRole,
    /// File name as the spec records it, including the extension.
    pub file: String,
    pub state: DictateModelState,
}

/// Everything preflight knows about dictation right now.
///
/// `models` is empty when `[dictate] enabled` is false, and preflight
/// then draws no Dictation section at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DictateSnapshot {
    pub models: Vec<DictateModel>,
    /// Set once, and never cleared: the only way past a failed preflight
    /// is a config edit or a fresh run.
    pub failure: Option<DictateFailure>,
}

impl DictateSnapshot {
    /// `true` when there is nothing left to wait for. Dictation that is
    /// switched off is trivially done; a failure never becomes done,
    /// which is what stops forge starting over a broken model.
    pub fn is_ready(&self) -> bool {
        self.failure.is_none() && self.models.iter().all(|m| m.state == DictateModelState::Ready)
    }
}

/// Preflight's own handle on the work: the snapshot the TUI renders and
/// the flag Escape sets.
pub(crate) struct DictateState {
    pub(crate) snapshot: Mutex<DictateSnapshot>,
    pub(crate) cancelled: AtomicBool,
    /// The live engine, held for the whole run. Dropping it unloads the
    /// weights, which is the one thing preflight exists to avoid.
    pub(crate) engine: Mutex<Option<Arc<forge_dictate::Engine>>>,
}

impl DictateState {
    pub(crate) fn new(settings: &DictateSettings) -> Self {
        let models = if settings.enabled { settings.initial_models() } else { Vec::new() };
        Self {
            snapshot: Mutex::new(DictateSnapshot { models, failure: None }),
            cancelled: AtomicBool::new(false),
            engine: Mutex::new(None),
        }
    }

    fn set_state(&self, file: &str, state: DictateModelState) {
        let mut snapshot = self.snapshot.lock();
        if let Some(model) = snapshot.models.iter_mut().find(|m| m.file == file) {
            model.state = state;
        }
    }

    fn fail(&self, failure: DictateFailure, file: Option<&str>) {
        if let Some(file) = file {
            self.set_state(file, DictateModelState::Failed);
        }
        self.snapshot.lock().failure = Some(failure);
    }
}

/// Fetch, verify and load every configured model, reporting progress
/// into `state` as it goes.
///
/// Both legs are blocking and documented as panicking on a runtime
/// thread in a debug build, so both run under `spawn_blocking`.
pub(crate) async fn run_dictate_preflight(settings: DictateSettings, state: Arc<DictateState>) {
    if !settings.enabled {
        return;
    }
    let cfg = settings.to_config();

    let prepare_state = Arc::clone(&state);
    let prepare_cfg = cfg.clone();
    let prepared = tokio::task::spawn_blocking(move || prepare(&prepare_cfg, &prepare_state)).await;
    match prepared {
        Ok(Ok(())) => {}
        Ok(Err(())) => return,
        Err(source) => {
            state.fail(DictateFailure::Other { message: source.to_string() }, None);
            return;
        }
    }

    for model in &mut state.snapshot.lock().models {
        model.state = DictateModelState::Loading;
    }

    // `Engine::new` returns in microseconds having handed the load to a
    // worker; `wait_ready` is the part that takes the second.
    let load_state = Arc::clone(&state);
    let load_cfg = cfg.clone();
    let loaded = tokio::task::spawn_blocking(move || {
        let engine = forge_dictate::Engine::new(cfg)?;
        engine.wait_ready()?;
        load_state.engine.lock().replace(engine);
        Ok::<(), forge_dictate::Error>(())
    })
    .await;

    match loaded {
        Ok(Ok(())) => {
            for model in &mut state.snapshot.lock().models {
                model.state = DictateModelState::Ready;
            }
        }
        Ok(Err(error)) => state.fail(failure_for(&load_cfg, &error), failing_file(&error)),
        Err(source) => state.fail(DictateFailure::Other { message: source.to_string() }, None),
    }
}

/// The fetch-and-verify leg. `Err(())` means `state` already carries the
/// reason.
fn prepare(cfg: &forge_dictate::Config, state: &DictateState) -> Result<(), ()> {
    use std::ops::ControlFlow;

    // What a `.part` held when the transfer opened, per file. A bar that
    // starts at 38% with nothing said about it reads as a bug.
    let mut resumed_from: Option<u64> = None;
    let mut in_flight: Option<(String, u64, u64)> = None;

    let outcome = forge_dictate::prepare(cfg, |progress| {
        if state.cancelled.load(Ordering::Relaxed) {
            return ControlFlow::Break(());
        }
        match progress {
            forge_dictate::Progress::Verifying { file } => {
                resumed_from = None;
                state.set_state(&file, DictateModelState::Verifying);
            }
            forge_dictate::Progress::Downloading { file, downloaded, total } => {
                // The first report of a transfer carries whatever was
                // already on disk, so it is the only chance to learn
                // that this is a resume rather than a fresh fetch.
                if in_flight.as_ref().is_none_or(|(name, ..)| name != &file) {
                    resumed_from = (downloaded > 0).then_some(downloaded);
                }
                in_flight = Some((file.clone(), downloaded, total));
                state.set_state(
                    &file,
                    DictateModelState::Downloading { downloaded, total, resumed_from },
                );
            }
            forge_dictate::Progress::Ready { file } => {
                resumed_from = None;
                in_flight = None;
                state.set_state(&file, DictateModelState::Fetched);
            }
        }
        ControlFlow::Continue(())
    });

    match outcome {
        Ok(()) => Ok(()),
        Err(forge_dictate::Error::Cancelled) => {
            let (file, kept, total) = in_flight.unwrap_or_default();
            state.fail(DictateFailure::Cancelled { kept, total }, Some(&file));
            Err(())
        }
        Err(error) => {
            state.fail(failure_for(cfg, &error), failing_file(&error));
            Err(())
        }
    }
}

/// The file an error is about, so the row that failed is the one that
/// goes red.
fn failing_file(error: &forge_dictate::Error) -> Option<&str> {
    use forge_dictate::Error as E;
    let (E::HashMismatch { path, .. }
    | E::SizeMismatch { path, .. }
    | E::StalePartial { path, .. }
    | E::ModelLoad { path, .. }
    | E::Io { path, .. }) = error
    else {
        return None;
    };
    // A rejected download is still a `.part`, and the row is keyed on
    // the finished name.
    path.file_name()?.to_str().map(|name| name.trim_end_matches(".part"))
}

/// Byte length the config records for the model at `path`, or zero when
/// the error is about a file no spec names.
fn spec_size(cfg: &forge_dictate::Config, path: &std::path::Path) -> u64 {
    let name = path.file_name().and_then(|n| n.to_str()).map(|n| n.trim_end_matches(".part"));
    std::iter::once(&cfg.asr_model)
        .chain(cfg.normalizer.as_ref())
        .find(|spec| Some(spec.file.as_str()) == name)
        .map_or(0, |spec| spec.size)
}

fn failure_for(cfg: &forge_dictate::Config, error: &forge_dictate::Error) -> DictateFailure {
    match error {
        forge_dictate::Error::HashMismatch { path, expected, actual } => {
            DictateFailure::HashMismatch {
                path: path.clone(),
                expected: expected.clone(),
                actual: actual.clone(),
                // A hash mismatch means the length already matched, so
                // the spec's size is what is on disk.
                size: spec_size(cfg, path),
            }
        }
        other => DictateFailure::Other { message: other.to_string() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An absent `[dictate]` section must not download three gigabytes
    /// because forge started, so the whole section defaults off.
    #[test]
    fn an_absent_section_is_disabled() {
        let settings: DictateSettings = toml::from_str("").expect("an empty section parses");
        assert!(!settings.enabled, "dictation must be opt-in, not implicit");
        assert_eq!(settings, DictateSettings::default());
    }

    /// A mistyped key silently doing nothing is worse here than a load
    /// failure: `modelsdir` would fetch 3 GB to the wrong volume and
    /// nothing would say so.
    #[test]
    fn a_mistyped_key_fails_the_load_rather_than_being_ignored() {
        let err = toml::from_str::<DictateSettings>("enabled = true\nmodelsdir = \"/vol\"\n")
            .expect_err("an unknown key must be refused");
        assert!(
            err.to_string().contains("modelsdir"),
            "the error must name the key that was not understood, got: {err}"
        );
    }

    /// The normalizer is the one exposed knob whose default is on, and
    /// turning it off is what halves the download - so it has to reach
    /// the config as an absent spec rather than as a present one.
    #[test]
    fn turning_the_normalizer_off_drops_its_model() {
        let on: DictateSettings = toml::from_str("enabled = true\n").expect("parse");
        assert!(on.to_config().normalizer.is_some(), "normalization is on by default");
        assert_eq!(on.initial_models().len(), 2, "both models are fetched by default");

        let off: DictateSettings =
            toml::from_str("enabled = true\nnormalizer = false\n").expect("parse");
        assert!(off.to_config().normalizer.is_none(), "normalizer = false must clear the spec");
        assert_eq!(
            off.initial_models().len(),
            1,
            "with no normalizer configured preflight has one model to fetch, not two"
        );
    }

    /// Minutes, because the value is a duration and TOML has no unit.
    /// Passed through as seconds, so a wrong multiplier would cap a
    /// recording at 30 seconds.
    #[test]
    fn max_capture_is_read_in_minutes() {
        let settings: DictateSettings =
            toml::from_str("enabled = true\nmax_capture_minutes = 5\n").expect("parse");
        assert_eq!(settings.to_config().max_capture, Duration::from_secs(300));
    }

    /// Preflight gates forge's boot on `is_ready`, so a failure must
    /// never satisfy it - not even one that arrives after every model
    /// has loaded.
    #[test]
    fn a_failure_is_never_ready() {
        let ready = DictateSnapshot {
            models: vec![DictateModel {
                role: DictateRole::Transcribing,
                file: "asr.gguf".to_owned(),
                state: DictateModelState::Ready,
            }],
            failure: None,
        };
        assert!(ready.is_ready(), "a loaded model with no failure is ready");

        let failed = DictateSnapshot {
            failure: Some(DictateFailure::Other { message: "nope".to_owned() }),
            ..ready.clone()
        };
        assert!(!failed.is_ready(), "a failure must hold the gate shut whatever the rows say");
    }

    /// Dictation that is switched off has nothing to wait for, and a
    /// gate that waited anyway would never open.
    #[test]
    fn a_disabled_dictation_is_immediately_ready() {
        let state = DictateState::new(&DictateSettings::default());
        let snapshot = state.snapshot.lock().clone();
        assert!(snapshot.models.is_empty(), "a disabled dictation draws no rows");
        assert!(snapshot.is_ready(), "with nothing configured there is nothing to wait for");
    }

    /// The row that failed is the one that has to go red, and a rejected
    /// download is reported against its `.part` while the row is keyed
    /// on the finished name.
    #[test]
    fn a_partial_rejection_is_attributed_to_its_finished_row() {
        let error = forge_dictate::Error::HashMismatch {
            path: PathBuf::from("/models/s1-mini-f16.gguf.part"),
            expected: "abc".to_owned(),
            actual: "def".to_owned(),
        };
        assert_eq!(
            failing_file(&error),
            Some("s1-mini-f16.gguf"),
            "a `.part` rejection must land on the row for the file it would have become",
        );
    }
}
