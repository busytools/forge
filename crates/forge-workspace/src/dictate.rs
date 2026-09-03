//! Dictation - the `[dictate]` section in `forge.toml`, and the
//! preflight pass that makes the models usable before forge starts.
//!
//! The costs are what shape this. A first run fetches 3.07 GB; every
//! later one re-hashes it, which is about 2.7 s for the pair now the
//! two models are prepared concurrently; loading the weights is another
//! second warm. None of that can happen while somebody is dictating, so
//! all of it happens once, at boot, on the preflight screen.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Deserialize;
use tokio::time::MissedTickBehavior;

use crate::SessionKey;
use crate::protocol::{DictateOutcome, SessionUpdate};

/// Cap on one recording when `[dictate] max_capture_minutes` is absent.
/// Matches the crate's own default; a capture reserves 4 bytes a sample
/// eagerly, so this is about 110 MiB held for the run.
const DEFAULT_MAX_CAPTURE_MINUTES: u64 = 30;

/// The push-to-talk key. Right Cmd is the default; on Linux and
/// Windows there is no Cmd key, so the cmd equivalent is the right
/// Control key, mirroring how the Cmd shortcuts accept Ctrl off macOS
/// (`is_cmd_shortcut`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DictateBind {
    #[default]
    RightCmd,
    LeftCmd,
    Off,
}

/// How a press/release pair maps onto starting and stopping a take.
/// `Auto` infers from timing; the forced modes bypass the tap window
/// entirely.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DictateMode {
    /// Sub-window tap toggles, hold transcribes on release.
    #[default]
    Auto,
    /// Press starts, the next press stops; releases never stop.
    Toggle,
    /// Hold records, release stops, however brief the hold.
    Hold,
}

impl DictateMode {
    /// What the forge.toml key accepts.
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Toggle => "toggle",
            Self::Hold => "hold",
        }
    }
}

impl<'de> Deserialize<'de> for DictateMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "auto" => Ok(Self::Auto),
            "toggle" => Ok(Self::Toggle),
            "hold" => Ok(Self::Hold),
            other => Err(serde::de::Error::unknown_variant(other, &["auto", "toggle", "hold"])),
        }
    }
}

/// What a session has overridden on the normalizer's prompt axes.
/// `None` on an axis means "the crate default"; the `/dictate` dialog
/// derives each row's in-force marker from this plus the defaults.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DictateOverrides {
    pub styling: Option<forge_dictate::normalize::Styling>,
    pub structure: Option<forge_dictate::normalize::Structure>,
    pub context: Option<forge_dictate::normalize::Context>,
}

impl DictateOverrides {
    /// The per-recording options this session dictates with: crate
    /// defaults with each overridden axis replaced.
    pub fn normalize_options(self) -> forge_dictate::NormalizeOptions {
        let mut options = forge_dictate::NormalizeOptions::default();
        if let Some(v) = self.styling {
            options.styling = v;
        }
        if let Some(v) = self.structure {
            options.structure = v;
        }
        if let Some(v) = self.context {
            options.context = v;
        }
        options
    }

    /// `true` when nothing is overridden: the reset row is inert and
    /// no row carries the session-set suffix.
    pub fn is_empty(self) -> bool {
        self.styling.is_none() && self.structure.is_none() && self.context.is_none()
    }
}

/// One edit the `/dictate` dialog asks for: set a single axis, or
/// clear them all. Enter on an already-set row re-sets the same value;
/// there is no per-axis clear, so the reset row is the only way back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictateOverrideUpdate {
    Styling(forge_dictate::normalize::Styling),
    Structure(forge_dictate::normalize::Structure),
    Context(forge_dictate::normalize::Context),
    Reset,
}

/// A session's pick in the `/dictate` overlay's device list. The
/// `forge.toml` `[dictate] device` pin is the default state, so it
/// needs no variant: the field is `None` until a pick lands. A pick
/// overrides the pin until the session ends - a restart reverts to the
/// pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DictateDeviceChoice {
    /// Record from the system default input for the rest of the
    /// session, whatever the pin names.
    System,
    /// Record from this device id.
    Device(String),
}

/// The input a capture records from: the session pick when one is set,
/// else the `[dictate] device` pin, else the system default. The
/// `None`/`Some` shape matches `forge_dictate::Config::device`, so the
/// result passes straight through `Engine::try_capture_with`. Shared
/// with the TUI, which resolves the same way to mark the device in
/// force.
pub fn resolve_capture_device(
    pick: Option<&DictateDeviceChoice>,
    configured: Option<&str>,
) -> Option<String> {
    match pick {
        Some(DictateDeviceChoice::System) => None,
        Some(DictateDeviceChoice::Device(id)) => Some(id.clone()),
        None => configured.map(str::to_owned),
    }
}

/// Everything the `/dictate` overlay's device block renders from: the
/// inputs a pick can offer and the configured pin the session pick
/// overrides. Resolved by [`crate::Workspace::dictate_device_catalog`].
#[derive(Debug, Clone)]
pub struct DictateDeviceCatalog {
    /// Every input present, in enumeration order.
    pub devices: Vec<forge_dictate::Device>,
    /// The `forge.toml` `[dictate] device` pin, if one is set.
    pub configured: Option<String>,
}

impl DictateBind {
    /// What the forge.toml key accepts.
    pub fn label(self) -> &'static str {
        match self {
            Self::RightCmd => "right_cmd",
            Self::LeftCmd => "left_cmd",
            Self::Off => "off",
        }
    }
}

impl<'de> Deserialize<'de> for DictateBind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "right_cmd" => Ok(Self::RightCmd),
            "left_cmd" => Ok(Self::LeftCmd),
            "off" => Ok(Self::Off),
            other => {
                Err(serde::de::Error::unknown_variant(other, &["right_cmd", "left_cmd", "off"]))
            }
        }
    }
}

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
    /// The push-to-talk key.
    #[serde(default)]
    pub bind: DictateBind,
    /// How press/release maps onto starting and stopping a take.
    #[serde(default)]
    pub mode: DictateMode,
}

fn enabled_by_default() -> bool {
    true
}

fn default_max_capture_minutes() -> u64 {
    DEFAULT_MAX_CAPTURE_MINUTES
}

#[cfg(test)]
mod default_max_capture_minutes_tests {
    /// The book (`docs/book/src/configuration.md`) documents `30` as
    /// the default; a drift here must fail a test, not the user's
    /// expectation.
    #[test]
    fn default_matches_the_documented_thirty_minutes() {
        assert_eq!(super::default_max_capture_minutes(), 30);
    }
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
            bind: DictateBind::default(),
            mode: DictateMode::default(),
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
    /// Set once, and never cleared for the run: nothing re-probes a
    /// failed model the way the account loader re-probes a bailed
    /// account, so clearing this one takes a config edit or a fresh run.
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

/// The config preflight builds the engine from: the `[dictate]`
/// settings plus the always-on diagnostics store under forge's
/// app-support dir, machine-local and never synced.
///
/// A directory that cannot be resolved turns the capture off with a
/// warning rather than failing preflight - diagnostics never break
/// dictation, but the absence must not be silent either: a capability
/// believed present and quietly missing is the failure nobody can see.
fn preflight_config(settings: &DictateSettings) -> forge_dictate::Config {
    let mut cfg = settings.to_config();
    match forge_sdk::app_support_dir() {
        Ok(dir) => cfg.diagnostics_dir = Some(dir.join("dictate-diagnostics")),
        Err(error) => tracing::warn!(%error, "dictate diagnostics off: no app-support dir"),
    }
    cfg
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
    let cfg = preflight_config(&settings);

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

/// Per-file transfer bookkeeping, kept across the progress events of one
/// `prepare` call.
///
/// Keyed by file throughout because the models are prepared concurrently
/// and their events interleave: a single slot lets one model's
/// `Verifying` clear the other's resume marker mid-transfer, and lets a
/// model that already finished be named as the one a cancellation
/// stopped.
#[derive(Default)]
struct TransferProgress {
    /// What a `.part` held when its transfer opened. A bar that starts
    /// at 38% with nothing said about it reads as a bug.
    resumed_from: HashMap<String, u64>,
    /// Bytes seen so far per file still transferring.
    in_flight: HashMap<String, (u64, u64)>,
    /// Which transfer a cancellation is reported against. With one model
    /// in flight this names the same file the old single slot did.
    cancelling: Option<String>,
}

impl TransferProgress {
    /// Fold one event in and return the row state it implies.
    fn apply(&mut self, progress: forge_dictate::Progress) -> (String, DictateModelState) {
        match progress {
            forge_dictate::Progress::Verifying { file } => {
                self.resumed_from.remove(&file);
                (file, DictateModelState::Verifying)
            }
            forge_dictate::Progress::Downloading { file, downloaded, total } => {
                // The first report of a transfer carries whatever was
                // already on disk, so it is the only chance to learn
                // that this is a resume rather than a fresh fetch.
                if !self.in_flight.contains_key(&file) && downloaded > 0 {
                    self.resumed_from.insert(file.clone(), downloaded);
                }
                self.in_flight.insert(file.clone(), (downloaded, total));
                self.cancelling = Some(file.clone());
                let resumed_from = self.resumed_from.get(&file).copied();
                (file, DictateModelState::Downloading { downloaded, total, resumed_from })
            }
            forge_dictate::Progress::Ready { file } => {
                self.resumed_from.remove(&file);
                self.in_flight.remove(&file);
                // A finished model is not what a later cancellation is
                // about. Leaving it named here reports a model that
                // downloaded and verified as cancelled, with the empty
                // byte counts of a transfer that is no longer running.
                if self.cancelling.as_deref() == Some(file.as_str()) {
                    self.cancelling = None;
                }
                (file, DictateModelState::Fetched)
            }
        }
    }

    /// The file a cancellation is about and how far it got. An empty name
    /// when nothing was transferring, which names no row and shows no
    /// bytes rather than blaming a model that finished.
    fn cancellation(&self) -> (String, u64, u64) {
        let file = self.cancelling.clone().unwrap_or_default();
        let (kept, total) = self.in_flight.get(&file).copied().unwrap_or_default();
        (file, kept, total)
    }
}

/// The fetch-and-verify leg. `Err(())` means `state` already carries the
/// reason.
fn prepare(cfg: &forge_dictate::Config, state: &DictateState) -> Result<(), ()> {
    use std::ops::ControlFlow;

    let mut transfers = TransferProgress::default();

    let outcome = forge_dictate::prepare(cfg, |progress| {
        if state.cancelled.load(Ordering::Relaxed) {
            return ControlFlow::Break(());
        }
        let (file, next) = transfers.apply(progress);
        state.set_state(&file, next);
        ControlFlow::Continue(())
    });

    match outcome {
        Ok(()) => Ok(()),
        Err(forge_dictate::Error::Cancelled) => {
            let (file, kept, total) = transfers.cancellation();
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

/// How often the live recording is polled for its level. A deliberate
/// fourth clock: coarser than the repaint gate, so every level event
/// lands on a repaint without driving it.
const METER_INTERVAL: Duration = Duration::from_millis(50);

/// How often a submitted take's per-window progress stream is polled.
/// A window takes seconds to decode; this cannot be felt.
const PROGRESS_POLL: Duration = Duration::from_millis(150);

/// The microphone side of one dictation: the session that started it
/// and the channel its recording task polls for the stop decision.
/// `true` on the channel means submit, `false` means abandon.
pub(crate) struct LiveRecording {
    pub(crate) key: SessionKey,
    pub(crate) stop: tokio::sync::mpsc::Sender<bool>,
}

/// A submitted take still awaiting its transcript. The microphone is
/// already released; the entry stays only so Esc can still abandon it.
pub(crate) struct FinishingTake {
    pub(crate) key: SessionKey,
    pub(crate) stop: tokio::sync::mpsc::Sender<bool>,
}

/// Process-wide dictation state. One microphone, so at most one
/// [`LiveRecording`]; several takes can be finishing at once, because a
/// new recording never waits for the previous transcript.
pub(crate) struct DictateRuntime {
    pub(crate) recording: Option<LiveRecording>,
    pub(crate) finishing: Vec<FinishingTake>,
    /// Handed out with every take's `DictateStarted` and echoed on its
    /// `DictateEnded`, so a resolver arriving after a newer take on the
    /// same key is recognisably stale. Starts at 1; 0 is the "matches
    /// nothing" value refusals carry.
    pub(crate) next_generation: u64,
    /// A stop that arrived before the start it answers had registered,
    /// stamped on arrival. `begin_capture` honours it only while it is
    /// fresh - the race it exists for is scheduler-scale, so a park
    /// older than the window is a stop whose take already resolved
    /// (a refusal, a cap self-submit) and must not poison the next
    /// attempt.
    pub(crate) stop_pending: Option<(SessionKey, Instant)>,
}

/// How long after parking a stop is still treated as racing its
/// start's registration. The gap it covers is the scheduler's delay
/// between two spawned tasks, so anything older is a stop whose take
/// is already gone.
const STOP_PARK_WINDOW: Duration = Duration::from_millis(200);

impl Default for DictateRuntime {
    fn default() -> Self {
        Self { recording: None, finishing: Vec::new(), next_generation: 1, stop_pending: None }
    }
}

impl DictateRuntime {
    /// The stop channel to route a `DictateStop` for `key` to, if a
    /// recording or a submitted take belongs to it.
    fn stop_channel_for(&self, key: &SessionKey) -> Option<tokio::sync::mpsc::Sender<bool>> {
        if let Some(recording) = self.recording.as_ref().filter(|r| &r.key == key) {
            return Some(recording.stop.clone());
        }
        self.finishing.iter().find(|take| &take.key == key).map(|take| take.stop.clone())
    }

    /// Consume `key`'s parked stop, answering whether it still races
    /// its start's registration and should pre-load an abandon. A
    /// stale park for the key is consumed without honour; another
    /// key's park is left alone.
    fn take_parked_stop(&mut self, key: &SessionKey, now: Instant) -> bool {
        let ours = self.stop_pending.as_ref().is_some_and(|(parked, _)| parked == key);
        if !ours {
            return false;
        }
        let fresh = self
            .stop_pending
            .as_ref()
            .is_some_and(|(_, at)| now.duration_since(*at) < STOP_PARK_WINDOW);
        self.stop_pending = None;
        fresh
    }

    /// Drop `key`'s parked stop, if any: the take it answered has
    /// resolved some other way, and a left-behind park would poison
    /// the session's next attempt.
    fn clear_stop_pending(&mut self, key: &SessionKey) {
        if self.stop_pending.as_ref().is_some_and(|(parked, _)| parked == key) {
            self.stop_pending = None;
        }
    }
}

/// `Command::DictateStart`: take the microphone for the composer at
/// `key` and start streaming level events.
pub(crate) async fn handle_dictate_start(ws: &Arc<crate::Workspace>, key: SessionKey) {
    let updates = ws.update_sender();
    let ws_for_capture = Arc::clone(ws);
    let key_for_capture = key.clone();
    let opened =
        tokio::task::spawn_blocking(move || begin_capture(&ws_for_capture, &key_for_capture)).await;
    match opened {
        Ok(Ok((capture, floor_db, generation, stop_rx))) => {
            let _ = updates.send(SessionUpdate::DictateStarted {
                key: key.clone(),
                floor_db,
                generation,
            });
            tokio::spawn(run_recording(Arc::clone(ws), key, capture, generation, stop_rx, updates));
        }
        Ok(Err(message)) => {
            // The start refused, so any park the press's release left
            // behind answers a take that will never exist.
            ws.dictate_runtime.lock().clear_stop_pending(&key);
            let _ = updates.send(SessionUpdate::DictateEnded {
                key,
                outcome: DictateOutcome::Refused { message },
                // Nothing started, so there is no generation to echo;
                // zero matches nothing a bucket could hold.
                generation: 0,
            });
        }
        Err(source) => {
            tracing::warn!(%source, "dictate start task failed to join");
        }
    }
}

/// `Command::DictateStop`: submit (`true`) or abandon the take that
/// `key` started. A stop that arrives before its start has registered
/// is parked in `stop_pending` for `begin_capture` to honour, rather
/// than dropped: dropping it would orphan a take until the capture cap
/// released the microphone. A stop whose take already resolved parks
/// too - the park cannot tell the two apart on arrival - but it is
/// only honoured while fresh, and every take-resolution path clears
/// it, so it never poisons the session's next attempt.
pub(crate) async fn handle_dictate_stop(
    ws: &Arc<crate::Workspace>,
    key: &SessionKey,
    submit: bool,
) {
    let stop = {
        let mut runtime = ws.dictate_runtime.lock();
        if let Some(stop) = runtime.stop_channel_for(key) {
            runtime.stop_pending = None;
            Some(stop)
        } else {
            runtime.stop_pending = Some((key.clone(), Instant::now()));
            None
        }
    };
    if let Some(stop) = stop {
        let _ = stop.send(submit).await;
    }
}

/// Take the microphone, refusing everything a host should say no to
/// before the first sample. Blocking (the device open waits on the
/// recorder thread), so it runs under `spawn_blocking`.
// The tuple is one take's handoff; a named type would say it once.
#[allow(clippy::type_complexity)]
fn begin_capture(
    ws: &Arc<crate::Workspace>,
    key: &SessionKey,
) -> Result<(forge_dictate::Capture, f32, u64, tokio::sync::mpsc::Receiver<bool>), String> {
    let (stop_tx, stop_rx) = tokio::sync::mpsc::channel(1);
    let mut runtime = ws.dictate_runtime.lock();
    if let Some(live) = runtime.recording.as_ref() {
        return Err(format!("the microphone is in use by session {}", live.key.as_str()));
    }
    let engine = ws
        .dictate
        .engine
        .lock()
        .clone()
        .ok_or("dictation is not ready · enable [dictate] in forge.toml and restart")?;
    // The session pick wins over the configured pin; with neither, the
    // system default records. Resolved before the open so a pin naming
    // a gone device errors here rather than falling back.
    let pick = ws.domain_session_for(key).and_then(|domain| domain.lock().dictate_device.clone());
    let wanted = crate::dictate::resolve_capture_device(pick.as_ref(), engine.device());
    let capture = engine
        .try_capture_with(key.as_str(), wanted.as_deref())
        .map_err(|busy| format!("the microphone is in use by session {}", busy.holder))?;
    // The session can close while the device open waits above. A
    // capture handed to a session that no longer exists holds the
    // microphone for nobody.
    if !ws.session_is_live(key) {
        drop(capture);
        return Err("the session closed · dictation did not start".to_owned());
    }
    if let Some(error) = capture.open_error() {
        tracing::warn!(?error, "dictation refused: the input device did not open");
        drop(capture);
        return Err("no input device is available · dictation did not start".to_owned());
    }
    runtime.recording = Some(LiveRecording { key: key.clone(), stop: stop_tx.clone() });
    // A stop the scheduler ordered before this registration is honoured
    // here: pre-load the abandon so the take ends the moment the
    // recording task starts reading its channel. A stale park - a stop
    // whose take resolved without it - is consumed without honour.
    if runtime.take_parked_stop(key, Instant::now()) {
        let _ = stop_tx.try_send(false);
    }
    let floor_db = engine.silence_floor();
    let generation = runtime.next_generation;
    runtime.next_generation = runtime.next_generation.wrapping_add(1);
    Ok((capture, floor_db, generation, stop_rx))
}

/// Own one take from first sample to delivered transcript: stream
/// level events while recording, then submit and wait, honouring the
/// stop channel at every phase.
async fn run_recording(
    ws: Arc<crate::Workspace>,
    key: SessionKey,
    capture: forge_dictate::Capture,
    generation: u64,
    mut stop: tokio::sync::mpsc::Receiver<bool>,
    updates: tokio::sync::mpsc::UnboundedSender<SessionUpdate>,
) {
    let submit = record_until_stopped(&key, &capture, &mut stop, &updates).await;
    if !submit {
        drop(capture);
        clear_recording_if_ours(&ws, &key);
        let _ = updates.send(SessionUpdate::DictateEnded {
            key,
            outcome: DictateOutcome::Cancelled,
            generation,
        });
        return;
    }

    let _ = updates.send(SessionUpdate::DictateTranscribing { key: key.clone() });
    // The take normalizes with the starting session's /dictate
    // overrides merged over the crate defaults; the session may have
    // closed since, in which case the defaults stand.
    let options = ws
        .domain_session_for(&key)
        .map(|domain| domain.lock().dictate_overrides.normalize_options())
        .unwrap_or_default();
    let Ok(mut ticket) = capture.finish_with(options) else {
        tracing::warn!("dictation could not submit its take");
        clear_recording_if_ours(&ws, &key);
        let _ = updates.send(SessionUpdate::DictateEnded {
            key,
            outcome: DictateOutcome::Failed,
            generation,
        });
        return;
    };
    // The microphone is released; the runtime entry moves from holding
    // the mic to awaiting the transcript so Esc still routes here.
    move_to_finishing(&ws, &key);

    // The token is cloned out before the ticket moves into the blocking
    // read, so abandoning this take touches only its own job - a queued
    // take behind it keeps its own token and aborts on its own turn.
    let cancel = ticket.cancel_token();
    // The per-window progress stream drains off the ticket before the
    // move; a single-window take emits one step, which the composer
    // renders as it always has.
    let mut progress = ticket.take_progress();
    let mut answer = tokio::task::spawn_blocking(move || ticket.recv());
    // Forward window steps as updates while the take decodes. The task
    // ends when the engine closes the stream at job end.
    let forward = updates.clone();
    let forward_key = key.clone();
    tokio::spawn(async move {
        let Some(progress) = progress.as_mut() else { return };
        let mut tick = tokio::time::interval(PROGRESS_POLL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            match progress.try_recv() {
                Ok(window_step) => {
                    if forward
                        .send(SessionUpdate::DictateProgress {
                            key: forward_key.clone(),
                            generation,
                            window: window_step.window,
                            total: window_step.total,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }
    });
    let outcome = match wait_for_take(&mut answer, &mut stop, &cancel).await {
        TakeResolution::Abandoned => DictateOutcome::Cancelled,
        TakeResolution::Answered(resolved) => match resolved {
            Ok(Ok(outcome)) => map_outcome(outcome),
            Ok(Err(error)) => {
                tracing::warn!(%error, "dictation failed");
                DictateOutcome::Failed
            }
            Err(source) => {
                tracing::warn!(%source, "dictation answer task failed to join");
                DictateOutcome::Failed
            }
        },
    };
    remove_finishing(&ws, &key);
    let _ = updates.send(SessionUpdate::DictateEnded { key, outcome, generation });
}

/// How waiting on a submitted take ended.
enum TakeResolution<T> {
    /// The transcript arrived, or the job failed - either way the take
    /// has an answer to report. `Err` is the blocking task's join
    /// failing, which reports as any other failure.
    Answered(Result<T, tokio::task::JoinError>),
    /// Esc or the owner going away abandoned the take. The caller
    /// reports [`DictateOutcome::Cancelled`] and the blocking answer,
    /// when it eventually lands, is dropped unread.
    Abandoned,
}

/// Wait for a submitted take's answer or a stop decision, whichever
/// comes first. A submit decision is a no-op (the take is already in),
/// and ANY other stop - `Some(false)` from Esc, or the channel closing
/// at teardown - fires this take's own cancel token and abandons the
/// wait. The abandon must end the wait rather than re-select: a closed
/// channel answers immediately and forever, and a loop that keeps
/// polling it busy-spins a core until the inference runs out on its
/// own.
async fn wait_for_take<T>(
    mut answer: &mut tokio::task::JoinHandle<T>,
    stop: &mut tokio::sync::mpsc::Receiver<bool>,
    cancel: &forge_dictate::CancelToken,
) -> TakeResolution<T> {
    loop {
        tokio::select! {
            resolved = &mut answer => break TakeResolution::Answered(resolved),
            decide = stop.recv() => {
                if decide != Some(true) {
                    cancel.cancel();
                    break TakeResolution::Abandoned;
                }
            }
        }
    }
}

/// Stream level events until a stop decision or the capture cap stops
/// itself. Returns whether the take should be submitted.
async fn record_until_stopped(
    key: &SessionKey,
    capture: &forge_dictate::Capture,
    stop: &mut tokio::sync::mpsc::Receiver<bool>,
    updates: &tokio::sync::mpsc::UnboundedSender<SessionUpdate>,
) -> bool {
    let mut meter = tokio::time::interval(METER_INTERVAL);
    meter.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = meter.tick() => {
                // The cap stopped the capture on its own; submitting is
                // what a release would have done.
                if capture.was_truncated() {
                    return true;
                }
                let peak_db = capture.level();
                let _ = updates.send(SessionUpdate::DictateLevel { key: key.clone(), peak_db });
            }
            decide = stop.recv() => {
                return decide.unwrap_or(false);
            }
        }
    }
}

/// Clear the recording slot only when it still belongs to `key` - a
/// teardown may already have removed it and a newer take from another
/// session may have installed its own, which is not ours to clear.
fn clear_recording_if_ours(ws: &crate::Workspace, key: &SessionKey) {
    let mut runtime = ws.dictate_runtime.lock();
    if runtime.recording.as_ref().is_some_and(|live| &live.key == key) {
        runtime.recording = None;
    }
    runtime.clear_stop_pending(key);
}

/// Move this take's stop channel from the recording slot to the
/// finishing list, so a stop during transcription still routes to it.
fn move_to_finishing(ws: &crate::Workspace, key: &SessionKey) {
    let mut runtime = ws.dictate_runtime.lock();
    if runtime.recording.as_ref().is_some_and(|live| &live.key == key)
        && let Some(live) = runtime.recording.take()
    {
        runtime.finishing.push(FinishingTake { key: key.clone(), stop: live.stop });
    }
}

/// Drop this take's finishing entry, whatever took it out of routing.
fn remove_finishing(ws: &crate::Workspace, key: &SessionKey) {
    let mut runtime = ws.dictate_runtime.lock();
    runtime.finishing.retain(|take| &take.key != key);
    runtime.clear_stop_pending(key);
}

/// Abandon everything `key` has in flight: a held microphone goes back
/// (dropping the entry closes the stop channel, which is what makes the
/// recording task release the device) and a submitted take loses its
/// cancel route. Called when the session closes.
pub(crate) fn teardown_for_closed_session(ws: &crate::Workspace, key: &SessionKey) {
    let mut runtime = ws.dictate_runtime.lock();
    if runtime.recording.as_ref().is_some_and(|live| &live.key == key) {
        runtime.recording = None;
    }
    runtime.finishing.retain(|take| &take.key != key);
    runtime.clear_stop_pending(key);
}

/// Release every session's dictation at once, for workspace shutdown.
pub(crate) fn teardown_all(ws: &crate::Workspace) {
    let mut runtime = ws.dictate_runtime.lock();
    runtime.recording = None;
    runtime.finishing.clear();
    runtime.stop_pending = None;
}

/// Map the crate's answer onto the wire outcome. A normalisation that
/// produced nothing is a valid answer, not a failure.
fn map_outcome(outcome: forge_dictate::Outcome) -> DictateOutcome {
    match outcome {
        forge_dictate::Outcome::Transcript(transcript) if transcript.text.is_empty() => {
            DictateOutcome::Empty
        }
        forge_dictate::Outcome::Transcript(transcript) => {
            DictateOutcome::Landed { text: transcript.text, truncated: transcript.truncated }
        }
        forge_dictate::Outcome::NoAudio { peak, audio } => {
            DictateOutcome::NoAudio { peak_db: peak, seconds: audio.as_secs() }
        }
    }
}

#[cfg(test)]
mod transfer_progress_tests {
    use super::*;
    use forge_dictate::Progress;

    /// The models are prepared concurrently, so a cancel can land while
    /// one is still transferring and the other has already finished.
    /// Naming the finished one puts `cancelled` in error red on a model
    /// that downloaded and hash-verified, under prose about a `.part`
    /// file it does not have.
    #[test]
    fn a_model_that_finished_is_not_named_by_a_later_cancellation() {
        let mut t = TransferProgress::default();
        t.apply(Progress::Downloading { file: "asr.gguf".into(), downloaded: 612, total: 1_560 });
        t.apply(Progress::Verifying { file: "asr.gguf".into() });
        t.apply(Progress::Ready { file: "asr.gguf".into() });
        // The other model is cached, so it only ever verifies.
        t.apply(Progress::Verifying { file: "norm.gguf".into() });

        let (file, kept, total) = t.cancellation();
        assert_eq!(file, "", "a model that reached ready must not be blamed for the cancel");
        assert_eq!((kept, total), (0, 0), "and it must not lend its byte counts to one");
    }

    /// The transfer that IS still running is the one to report, with its
    /// own numbers.
    #[test]
    fn the_transfer_still_running_is_the_one_reported() {
        let mut t = TransferProgress::default();
        t.apply(Progress::Downloading { file: "asr.gguf".into(), downloaded: 10, total: 100 });
        t.apply(Progress::Ready { file: "asr.gguf".into() });
        t.apply(Progress::Downloading { file: "norm.gguf".into(), downloaded: 40, total: 200 });

        assert_eq!(
            t.cancellation(),
            ("norm.gguf".to_owned(), 40, 200),
            "the live transfer and its own bytes, not the finished model's"
        );
    }

    /// Interleaved events must not let one model's `Verifying` clear the
    /// other's resume marker: the bar would silently drop the line
    /// explaining why it opened at 38%.
    #[test]
    fn one_models_verifying_does_not_clear_the_others_resume_marker() {
        let mut t = TransferProgress::default();
        t.apply(Progress::Downloading { file: "asr.gguf".into(), downloaded: 592, total: 1_560 });
        t.apply(Progress::Verifying { file: "norm.gguf".into() });
        let (_, state) = t.apply(Progress::Downloading {
            file: "asr.gguf".into(),
            downloaded: 600,
            total: 1_560,
        });

        assert_eq!(
            state,
            DictateModelState::Downloading {
                downloaded: 600,
                total: 1_560,
                resumed_from: Some(592)
            },
            "the resume marker belongs to asr.gguf and nothing norm.gguf does may clear it"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{Command, SessionUpdate};

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

    /// The preflight config always points the diagnostics store at
    /// forge's app-support dir. A refactor dropping that assignment
    /// would leave the feature shipped-dead with CI green, which is why
    /// the assembly is its own function and this pins it.
    #[test]
    fn preflight_config_points_diagnostics_at_app_support() {
        let settings: DictateSettings = toml::from_str("enabled = true\n").expect("parse");
        let cfg = preflight_config(&settings);
        assert_eq!(
            cfg.diagnostics_dir.as_deref(),
            forge_sdk::app_support_dir().ok().map(|dir| dir.join("dictate-diagnostics")).as_deref(),
            "the diagnostics store must sit beside forge's other machine-local state"
        );
        assert!(
            cfg.diagnostics_dir.is_some(),
            "on this machine an app-support dir resolves, so the store must be armed"
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

    /// The push-to-talk keybinding is a config knob, not a constant.
    /// Right Cmd is the default; `left_cmd` and `off` exist so the key
    /// can be moved or switched off, and a typo must fail the load the
    /// way every other `[dictate]` key does.
    #[test]
    fn bind_defaults_to_right_cmd_and_parses_the_three_values() {
        let settings: DictateSettings = toml::from_str("").expect("an empty section parses");
        assert_eq!(settings.bind, DictateBind::RightCmd);

        let left: DictateSettings = toml::from_str("bind = \"left_cmd\"\n").expect("parse");
        assert_eq!(left.bind, DictateBind::LeftCmd);

        let off: DictateSettings = toml::from_str("bind = \"off\"\n").expect("parse");
        assert_eq!(off.bind, DictateBind::Off);

        let err = toml::from_str::<DictateSettings>("bind = \"right_command\"\n")
            .expect_err("an unknown value must be refused");
        assert!(
            err.to_string().contains("right_command"),
            "the error must name the value that was not understood, got: {err}"
        );
    }

    /// The overlay's one reset control clears every axis, and the merged
    /// options must equal the crate defaults when nothing is set.
    #[test]
    fn normalize_options_merge_overridden_axes_over_the_crate_defaults() {
        let empty = DictateOverrides::default();
        let merged = empty.normalize_options();
        assert_eq!(merged, forge_dictate::NormalizeOptions::default());
        assert!(empty.is_empty(), "a fresh session has nothing to reset");

        let overridden = DictateOverrides {
            styling: Some(forge_dictate::normalize::Styling::Formal),
            ..DictateOverrides::default()
        };
        let merged = overridden.normalize_options();
        assert_eq!(merged.styling, forge_dictate::normalize::Styling::Formal);
        assert_eq!(
            merged.structure,
            forge_dictate::NormalizeOptions::default().structure,
            "an unset axis keeps the crate default"
        );
        assert!(!overridden.is_empty());
    }

    #[test]
    fn a_set_override_lands_on_its_own_session_and_echoes_back() {
        let (workspace, mut updates) = crate::Workspace::testing_stub();
        let a = crate::SessionKey::from_session_id("dictate-a");
        let b = crate::SessionKey::from_session_id("dictate-b");
        workspace.register_domain_session(a.clone(), None);
        workspace.register_domain_session(b.clone(), None);

        workspace
            .dispatch(Command::SetDictateOverride {
                key: a.clone(),
                update: DictateOverrideUpdate::Styling(forge_dictate::normalize::Styling::Formal),
            })
            .expect("dispatch");

        let a_binding = workspace.domain_session_for(&a).expect("session a");
        assert_eq!(
            a_binding.lock().dictate_overrides.styling,
            Some(forge_dictate::normalize::Styling::Formal),
        );
        let b_binding = workspace.domain_session_for(&b).expect("session b");
        assert_eq!(b_binding.lock().dictate_overrides, DictateOverrides::default());

        let echo = updates.try_recv().expect("an echo for the session that set it");
        match echo {
            SessionUpdate::DictateOverrides { key, overrides } => {
                assert_eq!(key, a);
                assert_eq!(overrides.styling, Some(forge_dictate::normalize::Styling::Formal));
            }
            other => panic!("unexpected update: {other:?}"),
        }
    }

    #[test]
    fn reset_clears_every_axis_at_once() {
        let (workspace, mut updates) = crate::Workspace::testing_stub();
        let key = crate::SessionKey::from_session_id("dictate-reset");
        workspace.register_domain_session(key.clone(), None);

        for update in [
            DictateOverrideUpdate::Styling(forge_dictate::normalize::Styling::Casual),
            DictateOverrideUpdate::Context(forge_dictate::normalize::Context::Email),
        ] {
            workspace
                .dispatch(Command::SetDictateOverride { key: key.clone(), update })
                .expect("dispatch");
        }
        workspace.dispatch(Command::ResetDictateOverrides { key: key.clone() }).expect("dispatch");

        let binding = workspace.domain_session_for(&key).expect("session");
        assert_eq!(binding.lock().dictate_overrides, DictateOverrides::default());

        let mut echoes = 0;
        while let Ok(update) = updates.try_recv() {
            if matches!(update, SessionUpdate::DictateOverrides { .. }) {
                echoes += 1;
            }
        }
        assert_eq!(echoes, 3, "each set echoes, and the reset does too");
    }

    #[test]
    fn an_override_for_an_unknown_session_is_refused() {
        let (workspace, _updates) = crate::Workspace::testing_stub();
        let key = crate::SessionKey::from_session_id("never-registered");
        let err = workspace
            .dispatch(Command::ResetDictateOverrides { key })
            .expect_err("an unknown session must be refused");
        assert!(matches!(err, crate::DispatchError::UnknownSession(_)));
    }

    #[test]
    fn a_device_pick_lands_on_the_session_and_echoes() {
        let (workspace, mut updates) = crate::Workspace::testing_stub();
        let key = crate::SessionKey::from_session_id("dictate-device");
        workspace.register_domain_session(key.clone(), None);

        workspace
            .dispatch(Command::SetDictateDevice {
                key: key.clone(),
                pick: Some(DictateDeviceChoice::Device("shure-id".into())),
            })
            .expect("dispatch");

        let binding = workspace.domain_session_for(&key).expect("session");
        assert_eq!(
            binding.lock().dictate_device,
            Some(DictateDeviceChoice::Device("shure-id".into())),
        );
        let echo = updates.try_recv().expect("a pin echo for the pick");
        match echo {
            SessionUpdate::DictateDevicePin { key: echoed, pick } => {
                assert_eq!(echoed, key);
                assert_eq!(pick, Some(DictateDeviceChoice::Device("shure-id".into())));
            }
            other => panic!("unexpected update: {other:?}"),
        }
    }

    #[test]
    fn reset_clears_the_device_pick_with_the_axes() {
        let (workspace, mut updates) = crate::Workspace::testing_stub();
        let key = crate::SessionKey::from_session_id("dictate-device-reset");
        workspace.register_domain_session(key.clone(), None);

        workspace
            .dispatch(Command::SetDictateDevice {
                key: key.clone(),
                pick: Some(DictateDeviceChoice::System),
            })
            .expect("dispatch");
        workspace.dispatch(Command::ResetDictateOverrides { key: key.clone() }).expect("dispatch");

        let binding = workspace.domain_session_for(&key).expect("session");
        assert_eq!(
            binding.lock().dictate_device,
            None,
            "back to defaults means back to the configured pin, so the pick must go"
        );

        let mut pin_echoes = vec![];
        while let Ok(update) = updates.try_recv() {
            if let SessionUpdate::DictateDevicePin { pick, .. } = update {
                pin_echoes.push(pick);
            }
        }
        assert_eq!(
            pin_echoes,
            vec![Some(DictateDeviceChoice::System), None],
            "the pick echoes when set, and the reset echoes the clear"
        );
    }

    #[test]
    fn the_capture_device_resolves_pick_over_pin_over_system() {
        let pick =
            |p: Option<DictateDeviceChoice>| resolve_capture_device(p.as_ref(), Some("config-id"));
        assert_eq!(
            pick(Some(DictateDeviceChoice::Device("pin-id".into()))).as_deref(),
            Some("pin-id"),
            "a session pick must beat the configured pin"
        );
        assert_eq!(
            pick(Some(DictateDeviceChoice::System)),
            None,
            "the system-default pick must override the configured pin too"
        );
        assert_eq!(
            pick(None).as_deref(),
            Some("config-id"),
            "no pick falls through to the configured pin"
        );
        assert_eq!(
            resolve_capture_device(None, None),
            None,
            "with neither, the system default records"
        );
    }

    /// The mode rides the same parsing rules as `bind`: three values,
    /// default `auto`, and a typo fails the load naming the value.
    #[test]
    fn mode_defaults_to_auto_and_parses_the_three_values() {
        let settings: DictateSettings = toml::from_str("").expect("an empty section parses");
        assert_eq!(settings.mode, DictateMode::Auto);

        let toggle: DictateSettings = toml::from_str("mode = \"toggle\"\n").expect("parse");
        assert_eq!(toggle.mode, DictateMode::Toggle);

        let hold: DictateSettings = toml::from_str("mode = \"hold\"\n").expect("parse");
        assert_eq!(hold.mode, DictateMode::Hold);

        let err = toml::from_str::<DictateSettings>("mode = \"press\"\n")
            .expect_err("an unknown value must be refused");
        assert!(
            err.to_string().contains("press"),
            "the error must name the value that was not understood, got: {err}"
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

#[cfg(test)]
mod dictate_lifecycle_tests {
    use super::*;

    fn key(name: &str) -> SessionKey {
        SessionKey::from_session_id(name.to_owned())
    }

    /// The meter cadence is a decided constant - a deliberate fourth
    /// clock, coarser than the repaint gate so every level event lands
    /// on a repaint. Pinning it makes a silent retune a failing test
    /// instead of a note nobody re-reads.
    #[test]
    fn the_meter_clock_is_the_decided_fifty_ms() {
        assert_eq!(
            METER_INTERVAL,
            Duration::from_millis(50),
            "the level meter runs at 20 Hz by decision, got {METER_INTERVAL:?}"
        );
    }

    /// The words a host inserts come from the transcript text; a
    /// normalisation that produced nothing is its own outcome, and the
    /// silence answer keeps its peak so a host can split a quiet room
    /// from no signal at all.
    #[test]
    fn crate_outcomes_map_onto_the_wire_outcomes() {
        use forge_dictate::Outcome;

        let landed = map_outcome(Outcome::Transcript(forge_dictate::Transcript {
            text: "hello there".to_owned(),
            asr: "hello there".to_owned(),
            stages: forge_dictate::Stages::default(),
            truncated: false,
        }));
        assert_eq!(
            landed,
            DictateOutcome::Landed { text: "hello there".to_owned(), truncated: false },
            "a transcript with words lands as words"
        );

        let empty = map_outcome(Outcome::Transcript(forge_dictate::Transcript {
            text: String::new(),
            asr: "um".to_owned(),
            stages: forge_dictate::Stages::default(),
            truncated: false,
        }));
        assert_eq!(empty, DictateOutcome::Empty, "filler-only input normalises to nothing");

        let silent = map_outcome(Outcome::NoAudio {
            peak: f32::NEG_INFINITY,
            audio: Duration::from_secs(4),
        });
        assert_eq!(
            silent,
            DictateOutcome::NoAudio { peak_db: f32::NEG_INFINITY, seconds: 4 },
            "the peak must survive so the host can tell quiet from structural silence"
        );
    }

    /// A start while another recording holds the microphone is refused
    /// before anything is opened, naming the holder.
    #[tokio::test]
    async fn a_start_while_the_microphone_is_held_names_the_holder() {
        let (ws, _updates) = crate::Workspace::testing_stub();
        let holder = key("holder-session");
        let (stop_tx, _stop_rx) = tokio::sync::mpsc::channel(1);
        ws.dictate_runtime.lock().recording =
            Some(LiveRecording { key: holder.clone(), stop: stop_tx });

        let Err(error) = begin_capture(&ws, &key("second-session")) else {
            panic!("the microphone is held, so a start must be refused");
        };
        assert!(
            error.contains("holder-session"),
            "the refusal must name the holding session, got: {error}"
        );
    }

    /// A start with no loaded engine is refused with the way out, not
    /// by touching the microphone.
    #[tokio::test]
    async fn a_start_without_an_engine_refuses_with_the_way_out() {
        let (ws, _updates) = crate::Workspace::testing_stub();
        assert!(
            ws.dictate.engine.lock().is_none(),
            "the stub preflight never ran, so no engine is loaded"
        );
        let Err(error) = begin_capture(&ws, &key("any")) else {
            panic!("no engine is loaded, so a start must be refused");
        };
        assert!(
            error.contains("not ready"),
            "the refusal must say dictation is not ready, got: {error}"
        );
    }

    /// The start command can lose a race with its own session closing -
    /// the device open waits inside `begin_capture` while the close
    /// gesture lands. A capture handed to a session that no longer
    /// exists holds the microphone for nobody.
    #[tokio::test]
    async fn a_start_for_a_closed_session_never_holds_the_microphone() {
        let (ws, _updates) = crate::Workspace::testing_stub();
        // A synthetic engine, so begin_capture gets past the open and
        // reaches the liveness check on every machine.
        let dir = tempfile::tempdir().unwrap();
        let engine = forge_dictate::test_support::engine_with_synthetic_microphone(
            forge_dictate::ConfigBuilder::new().models_dir(dir.path()).normalizer(None).build(),
        )
        .expect("an engine starts without its weights");
        *ws.dictate.engine.lock() = Some(engine);

        // No command sender for the key: the session is not live.
        match begin_capture(&ws, &key("ghost")) {
            Ok(_) => panic!("a capture must never be handed to a session that is not live"),
            Err(_) => assert!(
                ws.dictate_runtime.lock().recording.is_none(),
                "whatever refused the start, no microphone claim may survive it"
            ),
        }
    }

    /// Stop routing reaches the recording that owns the key, and an
    /// unknown key routes nowhere rather than to some other take.
    #[tokio::test]
    async fn a_stop_routes_to_the_take_that_started_on_the_key() {
        let (ws, _updates) = crate::Workspace::testing_stub();
        let owner = key("owner");
        let (stop_tx, mut stop_rx) = tokio::sync::mpsc::channel(1);
        ws.dictate_runtime.lock().recording = Some(LiveRecording { key: owner, stop: stop_tx });

        handle_dictate_stop(&ws, &key("owner"), true).await;
        assert!(
            matches!(stop_rx.try_recv(), Ok(true)),
            "submit must reach the owning take's channel"
        );

        handle_dictate_stop(&ws, &key("stranger"), false).await;
        assert!(
            matches!(stop_rx.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Empty)),
            "a stop from a session with no take must not reach someone else's"
        );
    }

    /// The stop channel follows the take from recording into finishing,
    /// so Esc can still abandon a submitted take whose transcript is in
    /// flight.
    #[tokio::test]
    async fn a_submitted_take_keeps_its_stop_channel_reachable() {
        let (ws, _updates) = crate::Workspace::testing_stub();
        let take = key("take");
        let (stop_tx, mut stop_rx) = tokio::sync::mpsc::channel(1);
        ws.dictate_runtime.lock().recording =
            Some(LiveRecording { key: take.clone(), stop: stop_tx });

        move_to_finishing(&ws, &take);
        {
            let runtime = ws.dictate_runtime.lock();
            assert!(runtime.recording.is_none(), "a submitted take no longer holds the microphone");
            assert_eq!(runtime.finishing.len(), 1, "the take is awaiting its transcript");
        }
        handle_dictate_stop(&ws, &take, false).await;
        assert!(
            matches!(stop_rx.try_recv(), Ok(false)),
            "abandon must still reach the take after it finished recording"
        );
    }

    /// A stop channel closed by teardown ends the wait and fires the
    /// take's own token. Before this was handled, the closed channel
    /// answered `None` immediately on every select iteration and the
    /// wait spun on one core until the inference ran out on its own.
    #[tokio::test]
    async fn a_closed_stop_channel_abandons_the_wait_instead_of_spinning() {
        // An answer that never lands: the worst case, the model does not
        // honour cancellation and the inference runs long. The sender is
        // released before the test ends, or the runtime drop would wait
        // out the blocking read.
        let (answer_tx, answer_rx) =
            std::sync::mpsc::channel::<Result<forge_dictate::Outcome, forge_dictate::Error>>();
        let mut answer = tokio::task::spawn_blocking(move || {
            answer_rx.recv().map_err(|_| forge_dictate::Error::EngineStopped)
        });
        let (stop_tx, mut stop_rx) = tokio::sync::mpsc::channel(1);
        drop(stop_tx);
        let cancel = forge_dictate::CancelToken::new();

        let started = std::time::Instant::now();
        let resolution = wait_for_take(&mut answer, &mut stop_rx, &cancel).await;
        drop(answer_tx);

        assert!(
            matches!(resolution, TakeResolution::Abandoned),
            "a closed channel means the owner is gone, not that the take resolved"
        );
        assert!(cancel.is_cancelled(), "the abandon fires this take's token");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the wait must end when the channel closes, not spin until the answer resolves"
        );
    }

    /// Esc during a transcription abandons the take and fires its
    /// token, whatever the answer is doing.
    #[tokio::test]
    async fn an_abandon_decision_fires_the_token_and_ends_the_wait() {
        let (answer_tx, answer_rx) =
            std::sync::mpsc::channel::<Result<forge_dictate::Outcome, forge_dictate::Error>>();
        let mut answer = tokio::task::spawn_blocking(move || {
            answer_rx.recv().map_err(|_| forge_dictate::Error::EngineStopped)
        });
        let (stop_tx, mut stop_rx) = tokio::sync::mpsc::channel(1);
        stop_tx.send(false).await.expect("the channel is open");
        let cancel = forge_dictate::CancelToken::new();

        let resolution = wait_for_take(&mut answer, &mut stop_rx, &cancel).await;
        drop(answer_tx);

        assert!(matches!(resolution, TakeResolution::Abandoned));
        assert!(cancel.is_cancelled(), "abandoning the take cancels its job");
    }

    /// A submit decision during a transcription is a no-op: the take is
    /// already submitted, so the wait continues to the answer.
    #[tokio::test]
    async fn a_submit_decision_during_transcription_keeps_waiting() {
        let (answer_tx, answer_rx) =
            std::sync::mpsc::channel::<Result<forge_dictate::Outcome, forge_dictate::Error>>();
        let mut answer = tokio::task::spawn_blocking(move || {
            answer_rx.recv().map_err(|_| forge_dictate::Error::EngineStopped)
        });
        let (stop_tx, mut stop_rx) = tokio::sync::mpsc::channel(1);
        stop_tx.send(true).await.expect("the channel is open");
        answer_tx
            .send(Err(forge_dictate::Error::EngineStopped))
            .expect("an unbounded channel accepts the answer");
        let cancel = forge_dictate::CancelToken::new();

        let resolution = wait_for_take(&mut answer, &mut stop_rx, &cancel).await;
        drop(answer_tx);

        match resolution {
            TakeResolution::Answered(_) => {
                assert!(!cancel.is_cancelled(), "a submit decision never cancels");
            }
            TakeResolution::Abandoned => {
                panic!("a submit decision during a transcription must keep waiting")
            }
        }
    }

    /// A stop the scheduler ordered before its start registered is
    /// parked, not dropped: dropping it orphans the take until the
    /// capture cap releases the microphone.
    #[tokio::test]
    async fn a_stop_before_its_start_is_parked_for_the_registration() {
        let (ws, _updates) = crate::Workspace::testing_stub();
        let owner = key("owner");

        handle_dictate_stop(&ws, &owner, true).await;
        assert!(
            matches!(&ws.dictate_runtime.lock().stop_pending, Some((parked, _)) if *parked == owner),
            "a stop with nothing to route parks itself for the start"
        );

        // A stop that DOES find a take neither parks nor leaves a stale
        // park behind.
        let (stop_tx, mut stop_rx) = tokio::sync::mpsc::channel(1);
        ws.dictate_runtime.lock().recording =
            Some(LiveRecording { key: owner.clone(), stop: stop_tx });
        handle_dictate_stop(&ws, &owner, true).await;
        assert_eq!(stop_rx.recv().await, Some(true));
        assert_eq!(ws.dictate_runtime.lock().stop_pending, None);
    }

    /// A parked stop is only honoured while fresh: the race it covers
    /// is the scheduler's gap between two spawned tasks, so a park
    /// older than the window is a stop whose take resolved some other
    /// way, and honouring it would waste the session's next attempt.
    #[test]
    fn a_stale_park_is_consumed_without_honour() {
        let mut runtime = DictateRuntime::default();
        let owner = key("owner");
        let stale = Instant::now()
            .checked_sub(STOP_PARK_WINDOW + Duration::from_millis(50))
            .expect("a process one window old can still backdate a park; boot is far older");
        runtime.stop_pending = Some((owner.clone(), stale));

        assert!(!runtime.take_parked_stop(&owner, Instant::now()));
        assert_eq!(runtime.stop_pending, None, "the stale park is consumed either way");

        runtime.stop_pending = Some((owner.clone(), Instant::now()));
        assert!(runtime.take_parked_stop(&owner, Instant::now()), "a fresh park is honoured");
        assert_eq!(runtime.stop_pending, None);

        // A park for a different key is never honoured and never
        // disturbs this key's own state.
        let other = key("other");
        runtime.stop_pending = Some((other.clone(), Instant::now()));
        assert!(!runtime.take_parked_stop(&owner, Instant::now()));
        assert!(runtime.stop_pending.is_some(), "the other key's park survives");
    }

    /// Teardown drops a session's park: an inert entry for a closed
    /// session would otherwise sit in the Option forever.
    #[tokio::test]
    async fn closing_the_session_clears_its_parked_stop() {
        let (ws, _updates) = crate::Workspace::testing_stub();
        let owner = key("owner");
        handle_dictate_stop(&ws, &owner, true).await;
        assert!(ws.dictate_runtime.lock().stop_pending.is_some());

        ws.release_session(&owner);

        assert_eq!(ws.dictate_runtime.lock().stop_pending, None);
    }

    /// A refused start clears the key's park: the release that follows
    /// the refusal parks AFTER this clear, and that later park is
    /// handled by the freshness window, but a park that arrived before
    /// the refusal must not survive it.
    #[tokio::test]
    async fn a_refused_start_clears_the_park() {
        let (ws, mut updates) = crate::Workspace::testing_stub();
        let owner = key("owner");
        ws.dictate_runtime.lock().stop_pending = Some((owner.clone(), Instant::now()));

        handle_dictate_start(&ws, owner.clone()).await;

        assert_eq!(ws.dictate_runtime.lock().stop_pending, None);
        let ended = updates.recv().await.expect("the refusal echo");
        assert!(matches!(
            ended,
            SessionUpdate::DictateEnded { outcome: DictateOutcome::Refused { .. }, .. }
        ));
    }

    /// Closing the session that owns a live take must release the
    /// microphone and cut the recording task's channel - otherwise the
    /// mic stays held and the meter keeps emitting for a composer that
    /// no longer exists, and every later start is refused naming a
    /// session the user closed.
    #[tokio::test]
    async fn closing_the_session_releases_its_live_take() {
        let (ws, _updates) = crate::Workspace::testing_stub();
        let owner = key("owner");
        let (stop_tx, mut stop_rx) = tokio::sync::mpsc::channel(1);
        ws.dictate_runtime.lock().recording =
            Some(LiveRecording { key: owner.clone(), stop: stop_tx });
        let (finishing_tx, _finishing_rx) = tokio::sync::mpsc::channel(1);
        ws.dictate_runtime.lock().finishing =
            vec![FinishingTake { key: owner.clone(), stop: finishing_tx }];

        ws.release_session(&owner);

        {
            let runtime = ws.dictate_runtime.lock();
            assert!(runtime.recording.is_none(), "a closed session must not keep the microphone");
            assert!(
                runtime.finishing.is_empty(),
                "a closed session's submitted take must not keep a cancel route"
            );
        }
        assert!(
            matches!(stop_rx.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)),
            "dropping the entry must close the channel, which is what makes the recording task abandon the take"
        );

        let Err(error) = begin_capture(&ws, &owner) else {
            panic!("dictation is not ready on the stub, so a start must be refused");
        };
        assert!(
            error.contains("not ready"),
            "a start after the close must fail on readiness, not on a dead session holding the mic: {error}"
        );
    }
}
