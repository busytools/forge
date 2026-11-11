//! Dictation's session-facing half: the `[dictate]` settings, the state
//! a host holds, the preflight pass that makes the models usable before
//! forge starts, and the live take lifecycle.
//!
//! The costs are what shape this. A first run fetches 3.07 GB; every
//! later one re-hashes what is on disk, about 0.8 s for the pair now the
//! two models are prepared concurrently, then loads the weights, another
//! second warm. The hash is skipped for a model whose size and
//! modification time still match the digest recorded for it, which is
//! every boot but the one after a file changed. None of that can happen
//! while somebody is dictating, so all of it happens once, at boot, on
//! the preflight screen.
//!
//! # The host seam
//!
//! Nothing here names a workspace type. The session it belongs to arrives
//! as [`DictateHost`], which the embedding workspace implements: the seat's
//! liveness, the app-support directory the preflight writes under, and one
//! `emit` for the updates a take produces. The updates are the crate's own
//! [`DictateUpdate`]; the host maps them onto its wire type. That is what
//! keeps this crate a leaf while the state and the policy move here.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Deserialize;
use tokio::time::MissedTickBehavior;

use forge_primitives::SessionSlot;

use crate::ModelSpec;

/// What one finished dictation take produced. Plain data rather than
/// [`crate::engine::Outcome`]: a view words the notices, so it gets the
/// observations and keeps the crate's error shapes.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DictateOutcome {
    /// Words to insert at the caret. `truncated` means the take hit the
    /// capture cap or the decode budget and is partial.
    Landed { text: String, truncated: bool },
    /// The audio normalised to nothing - a valid answer, not a failure.
    Empty,
    /// Nothing rose above the silence floor. A finite `peak_db` is a
    /// quiet room and a retry is reasonable; negative infinity means
    /// every sample was exactly zero, which is structural and sticky.
    NoAudio {
        #[serde(default, deserialize_with = "no_signal_reads_as_negative_infinity")]
        peak_db: f32,
        seconds: u64,
    },
    /// The take never happened. Covers a busy microphone, a device that
    /// would not open, and dictation not being ready.
    Refused { message: String },
    /// Recognition failed. The response is the same whichever way.
    Failed,
    /// The user abandoned the take. Resets silently.
    Cancelled,
}

/// Read a no-signal peak - a level's, or the one a `no_audio` outcome
/// carries - taking `null` as no signal rather than as a decode failure.
///
/// The counterpart of serde writing a non-finite float as `null`: a
/// window that heard nothing peaks at exactly zero, whose dBFS is
/// negative infinity. Without this, a Rust reader cannot decode the
/// message at all, and the failure names the type rather than the case.
///
/// Public because the wire's own `DictateLevel` carries the same field:
/// the workspace points its `deserialize_with` here rather than keeping a
/// second copy that can drift from this crate's.
pub fn no_signal_reads_as_negative_infinity<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<f32>::deserialize(deserializer)?.unwrap_or(f32::NEG_INFINITY))
}

/// One frame a take produces. The host maps each onto its own wire update;
/// none of them names a workspace type.
#[derive(Debug, Clone)]
pub enum DictateUpdate {
    /// One level reading for the recording at `key`.
    Level { key: SessionSlot, peak_db: f32, generation: u64, initiator: Option<u64> },
    /// The take was submitted and a transcript is in flight.
    Transcribing { key: SessionSlot, generation: u64, initiator: Option<u64> },
    /// The take settled `done` segments of its transcription.
    Progress {
        key: SessionSlot,
        generation: u64,
        done: usize,
        total: Option<usize>,
        initiator: Option<u64>,
    },
    /// The take is done: insert, notice or reset per `outcome`.
    Ended {
        key: SessionSlot,
        outcome: DictateOutcome,
        generation: u64,
        surface: Option<String>,
        initiator: Option<u64>,
    },
}

/// What the subsystem needs from the session it belongs to. The embedding
/// workspace implements this; the module never reaches past it.
pub trait DictateHost: Send + Sync {
    /// Whether the session behind `key` still exists.
    fn session_is_live(&self, key: &SessionSlot) -> bool;

    /// Forge's machine-local support directory, where the preflight's
    /// diagnostics and digest caches live. A host that cannot resolve one
    /// answers the cause, so the preflight's record carries what acting on
    /// it needs rather than a bare absence.
    fn app_support_dir(&self) -> Result<PathBuf, String>;

    /// Emit one update. `false` means no subscriber would take it, which
    /// ends a progress forwarder rather than looping forever.
    fn emit(&self, update: DictateUpdate) -> bool;
}

/// The model one role runs, and where that choice came from.
///
/// The catalogue and the install page build these; the preflight consumes
/// them. Kept here because `DictateState::active` is the module's own
/// state, and a workspace type in that field would put the crate's state
/// behind the workspace again.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveModel {
    pub role: DictateRole,
    pub spec: ModelSpec,
    pub from: ActiveFrom,
    /// RFC 3339, when a runtime pick chose it.
    pub at: Option<String>,
}

/// Where a role's model came from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum ActiveFrom {
    /// `forge.toml` names this role's model, so the runtime cannot change
    /// it; `key` is the `[dictate]` key that does.
    Config { key: String, variant: String },
    /// The last pick made on this machine.
    Installed { variant: String },
    /// The compiled default for this binary.
    Pin,
}

/// `~/` against the user's home directory. The workspace's config has a
/// byte-identical copy for its own settings; this one stays here because
/// the `[dictate]` settings are the module's, and a leaf cannot reach the
/// workspace's.
pub(crate) fn expand_home(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(stripped);
    }
    PathBuf::from(path)
}

/// Cap on one recording when `[dictate] max_capture_minutes` is absent.
/// Matches the crate's own default; a capture reserves 4 bytes a sample
/// eagerly, so this is about 110 MiB held for the run.
const DEFAULT_MAX_CAPTURE_MINUTES: u64 = 30;

/// The push-to-talk key. Right Cmd is the default; on Linux and
/// Windows there is no Cmd key, so the cmd equivalent is the right
/// Control key.
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DictateOverrides {
    pub styling: Option<crate::normalize::Styling>,
    pub structure: Option<crate::normalize::Structure>,
    pub context: Option<crate::normalize::Context>,
}

impl DictateOverrides {
    /// The per-recording options this session dictates with: crate
    /// defaults with each overridden axis replaced.
    pub fn normalize_options(self) -> crate::NormalizeOptions {
        let mut options = crate::NormalizeOptions::default();
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

/// The three axes decided, as a client that captures sends them: the
/// values it read from the greeting with its own edits applied, so every
/// axis is a value rather than a maybe. The crate defaults stand in where
/// nothing was configured, which is what the client's own panel draws as
/// the unset state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DictateAxes {
    pub styling: crate::normalize::Styling,
    pub structure: crate::normalize::Structure,
    pub context: crate::normalize::Context,
}

impl DictateAxes {
    /// The options a take with these axes normalizes by.
    pub fn normalize_options(self) -> crate::NormalizeOptions {
        crate::NormalizeOptions {
            styling: self.styling,
            structure: self.structure,
            context: self.context,
            ..Default::default()
        }
    }
}

impl Default for DictateAxes {
    fn default() -> Self {
        let options = crate::NormalizeOptions::default();
        Self { styling: options.styling, structure: options.structure, context: options.context }
    }
}

/// One edit the `/dictate` dialog asks for: set a single axis, or
/// clear them all. Enter on an already-set row re-sets the same value;
/// there is no per-axis clear, so the reset row is the only way back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DictateOverrideUpdate {
    Styling(crate::normalize::Styling),
    Structure(crate::normalize::Structure),
    Context(crate::normalize::Context),
    Reset,
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
/// Deliberately narrower than [`crate::Config`]. A model spec
/// carries a URL, a byte length and a digest, and hand-editing one is
/// how a file arrives that nothing can verify. The normalizer's three
/// prompt axes are permissions rather than instructions - they say what
/// the model MAY change, not what it must - so exposing them means
/// flipping one, seeing no difference, and concluding it is broken.
#[derive(Debug, Clone, Deserialize, PartialEq)]
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
    /// Ghost of the retired `[dictate] device` pin: read only so a stale
    /// forge.toml still carrying it warns at load instead of being refused
    /// as an unknown key. Capture moved to the client, which picks its own
    /// machine's input, so nothing reads this.
    #[serde(default)]
    pub device: Option<toml::Value>,
    /// The model the transcribing role runs, as the feed's own variant
    /// name. Set, it wins over every runtime pick and refuses Activate
    /// for the role; absent leaves the role to the last pick or the
    /// compiled pin.
    #[serde(default)]
    pub transcribe_model: Option<String>,
    /// The model the cleanup role runs, by feed variant name, under the
    /// same rule as [`Self::transcribe_model`]. Inert while
    /// [`Self::normalizer`] is off.
    #[serde(default)]
    pub cleanup_model: Option<String>,
    /// Spoken language hint. Absent autodetects.
    #[serde(default)]
    pub language: Option<String>,
    /// Rewrite recognition output into clean text. Off halves the
    /// download and skips a pass per utterance.
    #[serde(default = "enabled_by_default")]
    pub normalizer: bool,
    /// Upper bound on a single recording. A capture nobody stops ends
    /// itself here rather than holding the seat indefinitely.
    #[serde(default = "default_max_capture_minutes")]
    pub max_capture_minutes: u64,
    /// The push-to-talk key.
    #[serde(default)]
    pub bind: DictateBind,
    /// How press/release maps onto starting and stopping a take.
    #[serde(default)]
    pub mode: DictateMode,
    /// The register a take's cleanup rewrites into, where a client has not
    /// chosen its own. Absent means the crate's default.
    #[serde(default)]
    pub styling: Option<crate::normalize::Styling>,
    /// Whether the cleanup may return a bulleted list. Absent means the
    /// crate's default.
    #[serde(default)]
    pub structure: Option<crate::normalize::Structure>,
    /// What the words are being written for. Absent means the crate's
    /// default.
    #[serde(default)]
    pub context: Option<crate::normalize::Context>,
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
            transcribe_model: None,
            cleanup_model: None,
            language: None,
            normalizer: true,
            max_capture_minutes: DEFAULT_MAX_CAPTURE_MINUTES,
            bind: DictateBind::default(),
            mode: DictateMode::default(),
            styling: None,
            structure: None,
            context: None,
        }
    }
}

impl DictateSettings {
    /// The axes in force: what `forge.toml` sets, and the crate's own
    /// default wherever it sets nothing. This is what a client reads as the
    /// values it starts on and resets to.
    pub fn axes(&self) -> DictateAxes {
        let defaults = DictateAxes::default();
        DictateAxes {
            styling: self.styling.unwrap_or(defaults.styling),
            structure: self.structure.unwrap_or(defaults.structure),
            context: self.context.unwrap_or(defaults.context),
        }
    }

    /// The engine configuration these settings describe.
    pub fn to_config(&self) -> crate::Config {
        let mut builder = crate::ConfigBuilder::new()
            .max_capture(Duration::from_secs(self.max_capture_minutes.saturating_mul(60)));
        if let Some(dir) = self.models_dir.as_deref() {
            builder = builder.models_dir(expand_home(dir));
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
    pub fn models_dir(&self) -> Option<PathBuf> {
        match self.models_dir.as_deref() {
            Some(dir) => Some(expand_home(dir)),
            None => dirs::cache_dir().map(|dir| dir.join("forge-dictate")),
        }
    }

    /// The model each role runs, in role order: the pins plus the roles
    /// they run as. The one place the pair is spelled, so preflight's
    /// rows and the models page's IN USE rows cannot disagree about which
    /// models forge runs.
    pub fn model_specs(&self) -> Vec<(DictateRole, crate::ModelSpec)> {
        let cfg = self.to_config();
        std::iter::once((DictateRole::Transcribing, cfg.asr_model))
            .chain(cfg.normalizer.map(|spec| (DictateRole::Normalization, spec)))
            .collect()
    }

    /// The compiled pins as active models: what every role runs before any
    /// resolution has landed, and what a role with no key and no runtime
    /// pick keeps running.
    pub fn pin_active(&self) -> Vec<(DictateRole, ActiveModel)> {
        self.model_specs()
            .into_iter()
            .map(|(role, spec)| (role, ActiveModel { role, spec, from: ActiveFrom::Pin, at: None }))
            .collect()
    }

    /// The rows preflight draws before any work has started.
    fn initial_models(&self) -> Vec<DictateModel> {
        self.model_specs()
            .into_iter()
            .map(|(role, spec)| DictateModel {
                role,
                file: spec.file,
                state: DictateModelState::Pending,
            })
            .collect()
    }
}

/// What a model is for. Preflight labels the row by role and prints the
/// file underneath, because the role is what a reader is scanning for
/// and the file is the detail they want only when something is wrong.
///
/// Named for dictation, which owned the model lifecycle first; the set now
/// spans every local role, speech out included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DictateRole {
    Transcribing,
    Normalization,
    Speaking,
}

impl DictateRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::Transcribing => "transcribing model",
            Self::Normalization => "normalization model",
            Self::Speaking => "speaking model",
        }
    }
}

/// How far one model has got.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// The one that stopped preflight, carrying its own reason. The
    /// snapshot's copy is for the prose block under the rows.
    Failed(DictateFailure),
}

/// Why preflight stopped.
///
/// Every variant ends the run: there is no degraded dictation to fall
/// back to, so the screen names the way out and forge quits.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DictateSnapshot {
    pub models: Vec<DictateModel>,
    /// Set once, and never cleared for the run: nothing re-probes a
    /// failed model the way the account loader re-probes a bailed
    /// account, so clearing this one takes a config edit or a fresh run.
    /// The failing row carries its own copy; this one is the screen-wide
    /// prose block's.
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

/// Preflight's own handle on the work: the snapshot a view renders, the
/// cancellation flag, and the active models the resolution landed.
pub struct DictateState {
    pub snapshot: Mutex<DictateSnapshot>,
    pub cancelled: AtomicBool,
    /// The live engine, held for the whole run. Dropping it unloads the
    /// weights, which is the one thing preflight exists to avoid.
    pub engine: Mutex<Option<Arc<crate::Engine>>>,
    /// Which model each role runs: the compiled pins until the preflight
    /// resolves, then whatever the config, the runtime pick or the pin
    /// answered.
    pub active: Mutex<Vec<(DictateRole, ActiveModel)>>,
}

impl DictateState {
    pub fn new(settings: &DictateSettings) -> Self {
        let models = if settings.enabled { settings.initial_models() } else { Vec::new() };
        let active = if settings.enabled { settings.pin_active() } else { Vec::new() };
        Self {
            snapshot: Mutex::new(DictateSnapshot { models, failure: None }),
            cancelled: AtomicBool::new(false),
            engine: Mutex::new(None),
            active: Mutex::new(active),
        }
    }

    /// Move the resolved models in, before the preflight fetches anything:
    /// the rows follow the files the roles will actually run, keeping the
    /// state a row already reached when its file is unchanged.
    pub fn set_active(&self, models: &[(DictateRole, ActiveModel)]) {
        let rows = {
            let snapshot = self.snapshot.lock();
            models
                .iter()
                .map(|(role, model)| DictateModel {
                    role: *role,
                    file: model.spec.file.clone(),
                    state: snapshot
                        .models
                        .iter()
                        .find(|row| row.file == model.spec.file)
                        .map_or(DictateModelState::Pending, |row| row.state.clone()),
                })
                .collect()
        };
        self.snapshot.lock().models = rows;
        *self.active.lock() = models.to_vec();
    }

    fn set_state(&self, file: &str, state: DictateModelState) {
        let mut snapshot = self.snapshot.lock();
        if let Some(model) = snapshot.models.iter_mut().find(|m| m.file == file) {
            model.state = state;
        }
    }

    /// Move every row to one state: preflight's Loading and Ready points,
    /// and the same Ready after a runtime swap, where every role's model
    /// is loaded by the engine that just came up.
    pub fn mark_all(&self, state: &DictateModelState) {
        for model in &mut self.snapshot.lock().models {
            model.state = state.clone();
        }
    }

    pub fn fail(&self, failure: DictateFailure, file: Option<&str>) {
        if let Some(file) = file {
            self.set_state(file, DictateModelState::Failed(failure.clone()));
        }
        self.snapshot.lock().failure = Some(failure);
    }
}

/// The config preflight builds the engine from: the `[dictate]`
/// settings, the resolved active models, and the always-on stores under
/// forge's app-support dir - per-take diagnostics and the
/// verified-digest record - machine-local and never synced.
///
/// A directory that cannot be resolved turns the capture off with a
/// warning rather than failing preflight - diagnostics never break
/// dictation, but the absence must not be silent either: a capability
/// believed present and quietly missing is the failure nobody can see.
pub fn preflight_config(
    settings: &DictateSettings,
    active: &[(DictateRole, ActiveModel)],
    host: &dyn DictateHost,
) -> crate::Config {
    let mut cfg = settings.to_config();
    match host.app_support_dir() {
        Ok(dir) => {
            cfg.diagnostics_dir = Some(dir.join("dictate-diagnostics"));
            cfg.digest_cache_dir = Some(dir.join("dictate-digests"));
        }
        Err(error) => tracing::warn!(
            event_name = "dictate_state_dir_unresolved",
            %error,
            "no app-support dir: dictate per-take diagnostics and the model-digest record are off"
        ),
    }
    // The resolution replaces the pins the builder put in, per role.
    if let Some((_, model)) = active.iter().find(|(role, _)| *role == DictateRole::Transcribing) {
        cfg.asr_model = model.spec.clone();
    }
    cfg.normalizer = active
        .iter()
        .find(|(role, _)| *role == DictateRole::Normalization)
        .map(|(_, model)| model.spec.clone());
    cfg
}

/// Fetch, verify and load the active models, reporting progress into
/// `state` as it goes. The caller resolves the models and builds the
/// config; a dictation-off run never gets here.
///
/// Both legs are blocking and documented as panicking on a runtime
/// thread in a debug build, so both run under `spawn_blocking`.
pub async fn run_dictate_preflight(cfg: crate::Config, state: Arc<DictateState>) {
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

    state.mark_all(&DictateModelState::Loading);

    // `Engine::new` returns in microseconds having handed the load to a
    // worker; `wait_ready` is the part that takes the second.
    let load_state = Arc::clone(&state);
    let load_cfg = cfg.clone();
    let loaded = tokio::task::spawn_blocking(move || {
        let engine = crate::Engine::new(cfg)?;
        engine.wait_ready()?;
        load_state.engine.lock().replace(engine);
        Ok::<(), crate::Error>(())
    })
    .await;

    match loaded {
        Ok(Ok(())) => state.mark_all(&DictateModelState::Ready),
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
    fn apply(&mut self, progress: crate::Progress) -> (String, DictateModelState) {
        match progress {
            crate::Progress::Verifying { file } => {
                self.resumed_from.remove(&file);
                (file, DictateModelState::Verifying)
            }
            crate::Progress::Downloading { file, downloaded, total } => {
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
            crate::Progress::Ready { file } => {
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
fn prepare(cfg: &crate::Config, state: &DictateState) -> Result<(), ()> {
    use std::ops::ControlFlow;

    let mut transfers = TransferProgress::default();

    let outcome = crate::prepare(cfg, |progress| {
        if state.cancelled.load(Ordering::Relaxed) {
            return ControlFlow::Break(());
        }
        let (file, next) = transfers.apply(progress);
        state.set_state(&file, next);
        ControlFlow::Continue(())
    });

    match outcome {
        Ok(()) => Ok(()),
        Err(crate::Error::Cancelled) => {
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
fn failing_file(error: &crate::Error) -> Option<&str> {
    use crate::Error as E;
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
fn spec_size(cfg: &crate::Config, path: &std::path::Path) -> u64 {
    let name = path.file_name().and_then(|n| n.to_str()).map(|n| n.trim_end_matches(".part"));
    std::iter::once(&cfg.asr_model)
        .chain(cfg.normalizer.as_ref())
        .find(|spec| Some(spec.file.as_str()) == name)
        .map_or(0, |spec| spec.size)
}

fn failure_for(cfg: &crate::Config, error: &crate::Error) -> DictateFailure {
    match error {
        crate::Error::HashMismatch { path, expected, actual } => {
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

/// The live side of one dictation: the channel its task polls for the
/// stop decision (`true` on the channel means submit, `false` means
/// abandon), and - for a take the client captures - the sink the socket
/// pushes frames into. The seat is the map key, so the entry does not
/// carry it again.
pub struct LiveRecording {
    pub stop: tokio::sync::mpsc::Sender<bool>,
    /// `Some` on a take fed by frames the client sends; `None` on an
    /// entry that marks a take live without a socket feeding it.
    pub sink: Option<crate::FrameSink>,
    /// The connection this take belongs to, as its command carried it:
    /// every update about the take goes back there alone, and a stop or
    /// a close from any other connection is not this take's. `None`
    /// belongs to no connection.
    pub initiator: Option<u64>,
}

/// A submitted take still awaiting its transcript. The recording is
/// already stopped; the entry stays only so a stop can still abandon it.
pub struct FinishingTake {
    pub key: SessionSlot,
    pub stop: tokio::sync::mpsc::Sender<bool>,
    pub initiator: Option<u64>,
}

/// The read-aloud set being recorded, when one is: the audio a client's
/// own frames have fed so far, held until the stop that keeps or drops
/// it. It belongs to no session - the words are known, so nothing is
/// transcribed and nothing is routed - but it is a live capture like any
/// other, and the guards that wait on captures count it.
///
/// The connection that started it is the only one that can feed or stop
/// it, the same rule every framed take follows.
pub struct SetRecording {
    pub initiator: Option<u64>,
    pub samples: Vec<f32>,
}

/// Dictation state across every session. Every take is fed by a client's
/// own frames and holds no device, so several seats can dictate at once;
/// several takes can also be finishing, because a new recording never
/// waits for the previous transcript.
pub struct DictateRuntime {
    pub recordings: HashMap<SessionSlot, LiveRecording>,
    pub finishing: Vec<FinishingTake>,
    pub set_recording: Option<SetRecording>,
    /// Handed out with every take's `DictateStarted` and echoed on its
    /// `DictateEnded`, so a resolver arriving after a newer take on the
    /// same key is recognisably stale. Starts at 1; 0 is the "matches
    /// nothing" value refusals carry.
    pub next_generation: u64,
    /// A stop that arrived before the start it answers had registered,
    /// stamped on arrival and carrying its initiator so only a start by
    /// the same connection honours it. A registration honours it only
    /// while it is fresh - the race it exists for is scheduler-scale, so
    /// a park older than the window is a stop whose take already
    /// resolved (a refusal, a cap self-submit) and must not poison the
    /// next attempt.
    pub stop_pending: Option<(SessionSlot, Option<u64>, Instant)>,
}

/// How long after parking a stop is still treated as racing its
/// start's registration. The gap it covers is the scheduler's delay
/// between two spawned tasks, so anything older is a stop whose take
/// is already gone.
const STOP_PARK_WINDOW: Duration = Duration::from_millis(200);

impl Default for DictateRuntime {
    fn default() -> Self {
        Self {
            recordings: HashMap::new(),
            finishing: Vec::new(),
            set_recording: None,
            next_generation: 1,
            stop_pending: None,
        }
    }
}

impl DictateRuntime {
    /// The stop channel to route a `DictateStop` for `key` to, if a
    /// recording or a submitted take belongs to it AND to the connection
    /// that sent the stop: only the take's own connection can stop it.
    fn stop_channel_for(
        &self,
        key: &SessionSlot,
        initiator: Option<u64>,
    ) -> Option<tokio::sync::mpsc::Sender<bool>> {
        if let Some(recording) = self.recordings.get(key)
            && recording.initiator == initiator
        {
            return Some(recording.stop.clone());
        }
        self.finishing
            .iter()
            .find(|take| &take.key == key && take.initiator == initiator)
            .map(|take| take.stop.clone())
    }

    /// The seat holding a live capture, when one is: a recording still
    /// capturing, a submitted take still transcribing, or the read-aloud
    /// set being recorded. A model swap and a bench are refused while one
    /// is, and the refusal names this holder.
    pub fn live_holder(&self) -> Option<String> {
        let slot =
            self.recordings.keys().next().or_else(|| self.finishing.first().map(|take| &take.key));
        match slot {
            Some(slot) => Some(slot.display()),
            None if self.set_recording.is_some() => Some("the read-aloud recording".to_owned()),
            None => None,
        }
    }

    /// The live take a socket's frame belongs to, if `key` has one that
    /// the pushing connection started.
    ///
    /// **The take must be the pushing connection's own**, because a seat's
    /// live take can be another connection's - decision 3's own scenario,
    /// where a client whose start was refused still has its microphone open
    /// for the frames already on the wire. Answering `None` for a foreign
    /// take is what keeps one client's microphone out of another client's
    /// dictation.
    pub fn frame_sink_for(
        &self,
        key: &SessionSlot,
        initiator: Option<u64>,
    ) -> Option<&crate::FrameSink> {
        let live = self.recordings.get(key)?;
        if live.initiator != initiator {
            return None;
        }
        live.sink.as_ref()
    }

    /// Consume `key`'s parked stop, answering whether it still races
    /// its start's registration and should pre-load an abandon. A
    /// stale park for the key is consumed without honour; another
    /// key's - or another connection's - park is left alone.
    fn take_parked_stop(
        &mut self,
        key: &SessionSlot,
        initiator: Option<u64>,
        now: Instant,
    ) -> bool {
        let ours = self
            .stop_pending
            .as_ref()
            .is_some_and(|(parked, who, _)| parked == key && *who == initiator);
        if !ours {
            return false;
        }
        let fresh = self
            .stop_pending
            .as_ref()
            .is_some_and(|(_, _, at)| now.duration_since(*at) < STOP_PARK_WINDOW);
        self.stop_pending = None;
        fresh
    }

    /// Drop `key`'s parked stop, if any: the take it answered has
    /// resolved some other way, and a left-behind park would poison
    /// the session's next attempt.
    pub fn clear_stop_pending(&mut self, key: &SessionSlot, initiator: Option<u64>) {
        if self
            .stop_pending
            .as_ref()
            .is_some_and(|(parked, who, _)| parked == key && *who == initiator)
        {
            self.stop_pending = None;
        }
    }
}

/// The dictation subsystem's handle: preflight's state and the live
/// takes' runtime. The embedding workspace holds one of these and hands
/// it to the functions below; the host trait carries the session
/// callbacks those functions need.
pub struct Dictate {
    pub state: Arc<DictateState>,
    pub runtime: Mutex<DictateRuntime>,
}

impl Dictate {
    pub fn new(settings: &DictateSettings) -> Self {
        Self {
            state: Arc::new(DictateState::new(settings)),
            runtime: Mutex::new(DictateRuntime::default()),
        }
    }
}

/// A take the client will feed with frames, registered and ready to run:
/// what the socket pushes into, what the runner finishes, and what the
/// take normalizes by.
pub struct RegisteredTake {
    pub capture: crate::Capture,
    pub stop: tokio::sync::mpsc::Receiver<bool>,
    pub generation: u64,
    pub floor_db: f32,
    pub options: crate::NormalizeOptions,
    pub surface: Option<String>,
    pub initiator: Option<u64>,
}

/// `Command::DictateStream`: register a take the client will feed with
/// frames.
///
/// Registration is SYNCHRONOUS, and the dispatch arm performs it before
/// the runner is spawned: the socket's connection is one ordered stream,
/// so the frame a client sends right after its start message must find
/// the take already there. Nothing here opens a device, and that is what
/// makes the synchronous registration possible.
pub fn register_stream_take(
    host: &dyn DictateHost,
    dictate: &Dictate,
    key: &SessionSlot,
    axes: DictateAxes,
    surface: Option<String>,
    initiator: Option<u64>,
) -> Result<RegisteredTake, String> {
    let (stop_tx, stop_rx) = tokio::sync::mpsc::channel(1);
    let mut runtime = dictate.runtime.lock();
    if runtime.recordings.contains_key(key) {
        let ours = runtime.recordings.get(key).is_some_and(|held| held.initiator == initiator);
        return Err(if ours {
            "this client is already dictating on this session · dictation did not start".to_owned()
        } else {
            "another client is already dictating on this session · dictation did not start"
                .to_owned()
        });
    }
    // One live take per connection, because a frame carries no seat: with two
    // takes on one connection, a frame would have no stream it belonged to.
    if runtime.recordings.values().any(|live| live.sink.is_some() && live.initiator == initiator) {
        return Err(
            "this client is already dictating on another seat · dictation did not start".to_owned()
        );
    }
    let engine = dictate
        .state
        .engine
        .lock()
        .clone()
        .ok_or("dictation is not ready · enable [dictate] in forge.toml and restart")?;
    let stream = engine.capture_stream().map_err(|error| error.to_string())?;
    // The session can close between the press and this registration; a
    // take handed to a session that no longer exists is nobody's, and the
    // sink would hold frames for a seat no view is drawing.
    if !host.session_is_live(key) {
        return Err("the session closed · dictation did not start".to_owned());
    }
    runtime.recordings.insert(
        key.clone(),
        LiveRecording { stop: stop_tx.clone(), sink: Some(stream.sink), initiator },
    );
    // A stop the scheduler ordered before this registration is honoured
    // here: pre-load the abandon so the take ends the moment its runner
    // starts reading the channel.
    if runtime.take_parked_stop(key, initiator, Instant::now()) {
        let _ = stop_tx.try_send(false);
    }
    let floor_db = engine.silence_floor();
    let generation = runtime.next_generation;
    runtime.next_generation = runtime.next_generation.wrapping_add(1);
    Ok(RegisteredTake {
        capture: stream.capture,
        stop: stop_rx,
        generation,
        floor_db,
        options: axes.normalize_options(),
        surface,
        initiator,
    })
}

/// Own a stream-fed take from its registration to its delivered
/// transcript: the recording lifecycle, with the axes the client sent.
pub async fn run_stream_take(
    host: Arc<dyn DictateHost>,
    dictate: Arc<Dictate>,
    key: SessionSlot,
    take: RegisteredTake,
) {
    run_recording(
        host,
        dictate,
        key,
        take.capture,
        take.generation,
        take.stop,
        take.options,
        take.surface,
        take.initiator,
    )
    .await;
}

/// `Command::DictateStop`: submit (`true`) or abandon the take that
/// `key` started. A stop that arrives before its start has registered
/// is parked in `stop_pending` for the registration to honour, rather
/// than dropped: dropping it would orphan a take until the take's own
/// cap ended it. A stop whose take already resolved parks
/// too - the park cannot tell the two apart on arrival - but it is
/// only honoured while fresh, and every take-resolution path clears
/// it, so it never poisons the session's next attempt.
pub async fn handle_dictate_stop(
    dictate: &Dictate,
    key: &SessionSlot,
    submit: bool,
    initiator: Option<u64>,
) {
    let stop = {
        let mut runtime = dictate.runtime.lock();
        if let Some(stop) = runtime.stop_channel_for(key, initiator) {
            runtime.clear_stop_pending(key, initiator);
            Some(stop)
        } else {
            runtime.stop_pending = Some((key.clone(), initiator, Instant::now()));
            None
        }
    };
    if let Some(stop) = stop {
        let _ = stop.send(submit).await;
    }
}

/// Own one take from first sample to delivered transcript: stream
/// level events while recording, then submit and wait, honouring the
/// stop channel at every phase. Segments of a long take transcribe
/// while the recording is still running; the transcript lands once the
/// take's join has been normalized.
async fn run_recording(
    host: Arc<dyn DictateHost>,
    dictate: Arc<Dictate>,
    key: SessionSlot,
    mut capture: crate::Capture,
    generation: u64,
    mut stop: tokio::sync::mpsc::Receiver<bool>,
    options: crate::NormalizeOptions,
    surface: Option<String>,
    initiator: Option<u64>,
) {
    // Take progress spans the whole take: segments settle while the
    // recording is still open, and the tally gains its total after the
    // stop. The stream closes when the take's segmenter exits.
    tokio::spawn(forward_take_progress(
        Arc::clone(&host),
        key.clone(),
        generation,
        initiator,
        capture.take_progress(),
    ));
    // The meter handle, not the capture, crosses the await points: the
    // capture holds channel receivers and is not Sync.
    let meter = capture.meter();
    let cap = capture.max_capture();
    let submit =
        record_until_stopped(&key, &meter, &mut stop, host.as_ref(), cap, generation, initiator)
            .await;
    if !submit {
        drop(capture);
        clear_recording_if_ours(&dictate, &key, initiator);
        let _ = host.emit(DictateUpdate::Ended {
            key,
            outcome: DictateOutcome::Cancelled,
            generation,
            surface,
            initiator,
        });
        return;
    }

    let _ = host.emit(DictateUpdate::Transcribing { key: key.clone(), generation, initiator });
    let Ok(ticket) = capture.finish_with(options) else {
        tracing::warn!(
            event_name = "dictate_submit_failed",
            slot = %key.display(),
            "dictation could not submit its take"
        );
        clear_recording_if_ours(&dictate, &key, initiator);
        let _ = host.emit(DictateUpdate::Ended {
            key,
            outcome: DictateOutcome::Failed,
            generation,
            surface,
            initiator,
        });
        return;
    };
    // The recording is stopped; the runtime entry moves from the live
    // map to awaiting the transcript so Esc still routes here.
    move_to_finishing(&dictate, &key);

    // The token is cloned out before the ticket moves into the blocking
    // read, so abandoning this take touches only its own jobs - a
    // queued take behind it keeps its own token and aborts on its own
    // turn.
    let cancel = ticket.cancel_token();
    let mut answer = tokio::task::spawn_blocking(move || ticket.recv());
    let outcome = match wait_for_take(&mut answer, &mut stop, &cancel).await {
        TakeResolution::Abandoned => DictateOutcome::Cancelled,
        TakeResolution::Answered(resolved) => match resolved {
            Ok(Ok(outcome)) => map_outcome(outcome),
            Ok(Err(error)) => {
                tracing::debug!(
                    event_name = "dictate_failed",
                    slot = %key.display(),
                    %error,
                    "dictation failed"
                );
                DictateOutcome::Failed
            }
            Err(source) => {
                tracing::warn!(
                    event_name = "dictate_answer_task_unjoined",
                    slot = %key.display(),
                    %source,
                    "dictation answer task failed to join"
                );
                DictateOutcome::Failed
            }
        },
    };
    remove_finishing(&dictate, &key);
    let _ = host.emit(DictateUpdate::Ended { key, outcome, generation, surface, initiator });
}

/// Forward a take's progress steps as updates while it runs. Steps
/// arrive while the recording is still open (segments settling, no
/// total) and again after the stop (total known); the task ends when
/// the take's segmenter closes the stream.
async fn forward_take_progress(
    host: Arc<dyn DictateHost>,
    key: SessionSlot,
    generation: u64,
    initiator: Option<u64>,
    mut progress: Option<std::sync::mpsc::Receiver<crate::WindowProgress>>,
) {
    let Some(progress) = progress.as_mut() else { return };
    let mut tick = tokio::time::interval(PROGRESS_POLL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        match progress.try_recv() {
            Ok(step) => {
                let delivered = host.emit(DictateUpdate::Progress {
                    key: key.clone(),
                    generation,
                    done: step.done,
                    total: step.total,
                    initiator,
                });
                if !delivered {
                    break;
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        }
    }
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
///
/// The select is biased to the stop arm: when the answer lands in the
/// same poll as a queued stop, Esc wins and the answer is dropped
/// unread. An unbiased select picks a random ready arm, which landed
/// a cancelled take about half the time.
async fn wait_for_take<T>(
    answer: &mut tokio::task::JoinHandle<T>,
    stop: &mut tokio::sync::mpsc::Receiver<bool>,
    cancel: &crate::CancelToken,
) -> TakeResolution<T> {
    loop {
        tokio::select! {
            biased;
            decide = stop.recv() => {
                if decide != Some(true) {
                    cancel.cancel();
                    break TakeResolution::Abandoned;
                }
            }
            resolved = &mut *answer => break TakeResolution::Answered(resolved),
        }
    }
}

/// Stream level events until a stop decision or the capture cap stops
/// itself. Returns whether the take should be submitted.
///
/// `cap` is enforced HERE as well as in the capture, by wall clock: the
/// sample cap fires only while audio arrives, so a take whose stream stops
/// mid-take has nothing to notice it and would hold the seat until
/// teardown.
///
/// The select is biased to the stop arm: a stop queued when a meter tick
/// is ready in the same poll cancels the take, so the cap cannot submit
/// one the user already cancelled.
async fn record_until_stopped(
    key: &SessionSlot,
    capture: &crate::CaptureMeter,
    stop: &mut tokio::sync::mpsc::Receiver<bool>,
    host: &dyn DictateHost,
    cap: Duration,
    generation: u64,
    initiator: Option<u64>,
) -> bool {
    let deadline = Instant::now() + cap;
    let mut meter = tokio::time::interval(METER_INTERVAL);
    meter.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            decide = stop.recv() => {
                return decide.unwrap_or(false);
            }
            _ = meter.tick() => {
                // The cap stopped the capture on its own; submitting is
                // what a release would have done.
                if capture.was_truncated() {
                    return true;
                }
                if Instant::now() >= deadline {
                    capture.cap_reached();
                    return true;
                }
                let peak_db = capture.level();
                let _ = host.emit(DictateUpdate::Level {
                    key: key.clone(),
                    peak_db,
                    generation,
                    initiator,
                });
            }
        }
    }
}

/// Clear the seat's live entry when the take that owns it has resolved -
/// a teardown may already have removed it, which this tolerates.
fn clear_recording_if_ours(dictate: &Dictate, key: &SessionSlot, initiator: Option<u64>) {
    let mut runtime = dictate.runtime.lock();
    runtime.recordings.remove(key);
    runtime.clear_stop_pending(key, initiator);
}

/// Move this take's stop channel from the live map to the finishing
/// list, so a stop during transcription still routes to it.
///
/// Public so a host's integration tests can drive the finishing path the
/// way a stop during transcription reaches it.
pub fn move_to_finishing(dictate: &Dictate, key: &SessionSlot) {
    let mut runtime = dictate.runtime.lock();
    if let Some(live) = runtime.recordings.remove(key) {
        runtime.finishing.push(FinishingTake {
            key: key.clone(),
            stop: live.stop,
            initiator: live.initiator,
        });
    }
}

/// Drop this take's finishing entry, whatever took it out of routing.
fn remove_finishing(dictate: &Dictate, key: &SessionSlot) {
    let mut runtime = dictate.runtime.lock();
    runtime.finishing.retain(|take| &take.key != key);
}

/// Abandon everything `key` has in flight: a live take goes back
/// (dropping the entry closes the stop channel, which is what makes its
/// runner abandon it) and a submitted take loses its cancel route.
/// Called when the session closes.
pub fn teardown_for_closed_session(dictate: &Dictate, key: &SessionSlot) {
    let mut runtime = dictate.runtime.lock();
    runtime.recordings.remove(key);
    runtime.finishing.retain(|take| &take.key != key);
    // A park for the closing seat is moot whoever left it.
    if runtime.stop_pending.as_ref().is_some_and(|(parked, _, _)| parked == key) {
        runtime.stop_pending = None;
    }
}

/// Release every session's dictation at once, for workspace shutdown.
pub fn teardown_all(dictate: &Dictate) {
    let mut runtime = dictate.runtime.lock();
    runtime.recordings.clear();
    runtime.finishing.clear();
    runtime.stop_pending = None;
}

/// Map the crate's answer onto the wire outcome. A normalisation that
/// produced nothing is a valid answer, not a failure.
fn map_outcome(outcome: crate::Outcome) -> DictateOutcome {
    match outcome {
        crate::Outcome::Transcript(transcript) if transcript.text.is_empty() => {
            DictateOutcome::Empty
        }
        crate::Outcome::Transcript(transcript) => {
            DictateOutcome::Landed { text: transcript.text, truncated: transcript.truncated }
        }
        crate::Outcome::NoAudio { peak, audio } => {
            DictateOutcome::NoAudio { peak_db: peak, seconds: audio.as_secs() }
        }
    }
}

#[cfg(test)]
mod transfer_progress_tests {
    use super::*;
    use crate::Progress;

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

    /// The axes are `forge.toml` keys and the crate's own defaults stand
    /// in where they are absent - which is what a client draws as its
    /// unset state and resets to.
    #[test]
    fn the_axes_come_from_the_config_over_the_crates_own_defaults() {
        let defaults: DictateSettings = toml::from_str("").expect("an empty section parses");
        assert_eq!(
            defaults.axes(),
            DictateAxes::default(),
            "with nothing configured the axes are the crate's defaults"
        );

        let set: DictateSettings =
            toml::from_str("styling = \"formal\"\nstructure = \"lists\"\ncontext = \"email\"\n")
                .expect("the axes parse");
        let axes = set.axes();
        assert_eq!(axes.styling, crate::normalize::Styling::Formal);
        assert_eq!(axes.structure, crate::normalize::Structure::Lists);
        assert_eq!(axes.context, crate::normalize::Context::Email);

        // A typo fails the load the way every other `[dictate]` key does.
        let err = toml::from_str::<DictateSettings>("styling = \"chatty\"\n")
            .expect_err("an unknown value must be refused");
        assert!(err.to_string().contains("chatty"), "the error must name the value, got: {err}");
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
        assert_eq!(merged, crate::NormalizeOptions::default());
        assert!(empty.is_empty(), "a fresh session has nothing to reset");

        let overridden = DictateOverrides {
            styling: Some(crate::normalize::Styling::Formal),
            ..DictateOverrides::default()
        };
        let merged = overridden.normalize_options();
        assert_eq!(merged.styling, crate::normalize::Styling::Formal);
        assert_eq!(
            merged.structure,
            crate::NormalizeOptions::default().structure,
            "an unset axis keeps the crate default"
        );
        assert!(!overridden.is_empty());
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

    /// `fail` is the only production writer of `Failed`, so both writes
    /// it makes are load-bearing: the row must carry its own reason and
    /// the snapshot must keep the screen-wide copy.
    #[test]
    fn fail_marks_the_row_and_keeps_the_screen_wide_copy() {
        let settings: DictateSettings = toml::from_str("enabled = true\n").expect("parse");
        let state = DictateState::new(&settings);
        let file = state.snapshot.lock().models[0].file.clone();

        let failure = DictateFailure::Cancelled { kept: 100, total: 200 };
        state.fail(failure.clone(), Some(&file));

        let snapshot = state.snapshot.lock().clone();
        assert_eq!(
            snapshot.models.iter().find(|m| m.file == file).map(|m| &m.state),
            Some(&DictateModelState::Failed(failure.clone())),
            "the row that stopped carries its own reason",
        );
        assert_eq!(
            snapshot.failure,
            Some(failure),
            "and the snapshot keeps the screen-wide copy for the prose block",
        );
    }

    /// The row that failed is the one that has to go red, and a rejected
    /// download is reported against its `.part` while the row is keyed
    /// on the finished name.
    #[test]
    fn a_partial_rejection_is_attributed_to_its_finished_row() {
        let error = crate::Error::HashMismatch {
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

    fn key(name: &str) -> SessionSlot {
        SessionSlot::new("TestOrg", "test-project", name)
    }

    /// A host that accepts every update and reports every seat live: the
    /// policy tests need the seam's shape, not a workspace behind it.
    struct TestHost;

    impl DictateHost for TestHost {
        fn session_is_live(&self, _key: &SessionSlot) -> bool {
            true
        }

        fn app_support_dir(&self) -> Result<PathBuf, String> {
            Err("the test host has no app-support directory".to_owned())
        }

        fn emit(&self, _update: DictateUpdate) -> bool {
            true
        }
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
        use crate::Outcome;

        let landed = map_outcome(Outcome::Transcript(crate::Transcript {
            text: "hello there".to_owned(),
            asr: "hello there".to_owned(),
            stages: crate::Stages::default(),
            truncated: false,
        }));
        assert_eq!(
            landed,
            DictateOutcome::Landed { text: "hello there".to_owned(), truncated: false },
            "a transcript with words lands as words"
        );

        let empty = map_outcome(Outcome::Transcript(crate::Transcript {
            text: String::new(),
            asr: "um".to_owned(),
            stages: crate::Stages::default(),
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

    /// The axes a client sends decide its take's normalizer options,
    /// where a `None` axis would have meant "the crate default".
    #[test]
    fn the_axes_a_client_sends_decide_its_takes_options() {
        let axes = DictateAxes {
            styling: crate::normalize::Styling::Formal,
            structure: crate::normalize::Structure::Lists,
            context: crate::normalize::Context::Email,
        };
        let options = axes.normalize_options();
        assert_eq!(options.styling, crate::normalize::Styling::Formal);
        assert_eq!(options.structure, crate::normalize::Structure::Lists);
        assert_eq!(options.context, crate::normalize::Context::Email);

        let defaults = DictateAxes::default().normalize_options();
        assert_eq!(
            defaults,
            crate::NormalizeOptions::default(),
            "an unset client must normalize exactly as the crate's own defaults do"
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
            std::sync::mpsc::channel::<Result<crate::Outcome, crate::Error>>();
        let mut answer = tokio::task::spawn_blocking(move || {
            answer_rx.recv().map_err(|_| crate::Error::EngineStopped)
        });
        let (stop_tx, mut stop_rx) = tokio::sync::mpsc::channel(1);
        drop(stop_tx);
        let cancel = crate::CancelToken::new();

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
            std::sync::mpsc::channel::<Result<crate::Outcome, crate::Error>>();
        let mut answer = tokio::task::spawn_blocking(move || {
            answer_rx.recv().map_err(|_| crate::Error::EngineStopped)
        });
        let (stop_tx, mut stop_rx) = tokio::sync::mpsc::channel(1);
        stop_tx.send(false).await.expect("the channel is open");
        let cancel = crate::CancelToken::new();

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
            std::sync::mpsc::channel::<Result<crate::Outcome, crate::Error>>();
        let mut answer = tokio::task::spawn_blocking(move || {
            answer_rx.recv().map_err(|_| crate::Error::EngineStopped)
        });
        let (stop_tx, mut stop_rx) = tokio::sync::mpsc::channel(1);
        stop_tx.send(true).await.expect("the channel is open");
        answer_tx
            .send(Err(crate::Error::EngineStopped))
            .expect("an unbounded channel accepts the answer");
        let cancel = crate::CancelToken::new();

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

    /// When the answer lands in the same poll as a queued stop, the
    /// stop wins: Esc must cancel the take rather than land words the
    /// user already abandoned. An unbiased select picks a random ready
    /// arm, so this resolved as the answer about half the time.
    #[tokio::test]
    async fn a_queued_stop_beats_an_answer_that_is_already_ready() {
        // Tokio redraws the start index on every select invocation, so
        // rounds are independent coin flips: the mutant failed 10 of 10
        // runs, first fail on round 3, 0, 0, 1, 0, 1, 2, 1, 2, 2. 64
        // rounds put a false pass at 2^-64.
        for round in 0..64u32 {
            let mut answer = tokio::spawn(async move { round });
            while !answer.is_finished() {
                tokio::task::yield_now().await;
            }
            let (stop_tx, mut stop_rx) = tokio::sync::mpsc::channel(1);
            stop_tx.send(false).await.expect("the channel is open");
            let cancel = crate::CancelToken::new();

            let resolution = wait_for_take(&mut answer, &mut stop_rx, &cancel).await;

            assert!(
                matches!(resolution, TakeResolution::Abandoned),
                "round {round}: the take answered although the user's stop was queued"
            );
            assert!(cancel.is_cancelled(), "round {round}: the abandon fires this take's token");
        }
    }

    /// A stop queued when the cap tick fires must cancel the take, not
    /// submit it: the tick's submit path returns what the user already
    /// refused. Under the paused clock the interval's first tick and a
    /// zero cap are both ready on the first poll, so the two arms race
    /// exactly as they do at the cap boundary.
    #[tokio::test(start_paused = true)]
    async fn a_queued_stop_beats_a_cap_tick_that_is_already_ready() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::ConfigBuilder::new().models_dir(dir.path()).normalizer(None).build();
        let engine = crate::Engine::new(cfg).expect("engine must start");
        let stream = engine.capture_stream().expect("a stream take starts");
        let meter = stream.capture.meter();
        let host = TestHost;
        let key = key("capped");

        // The same per-invocation redraw makes each round an independent
        // coin flip; 64 rounds put a false pass at 2^-64.
        for round in 0..64u32 {
            let (stop_tx, mut stop_rx) = tokio::sync::mpsc::channel(1);
            stop_tx.send(false).await.expect("the channel is open");

            let submitted =
                record_until_stopped(&key, &meter, &mut stop_rx, &host, Duration::ZERO, 0, None)
                    .await;

            assert!(!submitted, "round {round}: the cap submitted a take the user cancelled");
        }
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
        runtime.stop_pending = Some((owner.clone(), None, stale));

        assert!(!runtime.take_parked_stop(&owner, None, Instant::now()));
        assert_eq!(runtime.stop_pending, None, "the stale park is consumed either way");

        runtime.stop_pending = Some((owner.clone(), None, Instant::now()));
        assert!(runtime.take_parked_stop(&owner, None, Instant::now()), "a fresh park is honoured");
        assert_eq!(runtime.stop_pending, None);

        // A park for a different key is never honoured and never
        // disturbs this key's own state.
        let other = key("other");
        runtime.stop_pending = Some((other.clone(), None, Instant::now()));
        assert!(!runtime.take_parked_stop(&owner, None, Instant::now()));
        assert!(runtime.stop_pending.is_some(), "the other key's park survives");

        // Nor is a park for another connection: a stop belongs to the
        // take its own connection started.
        runtime.stop_pending = Some((owner.clone(), Some(9), Instant::now()));
        assert!(
            !runtime.take_parked_stop(&owner, None, Instant::now()),
            "another connection's park is not this start's to honour"
        );
        assert!(runtime.stop_pending.is_some(), "and it survives for its own start");
    }

    /// A test host that reports one app-support directory, so the
    /// preflight's assembly can be pinned without a workspace.
    struct DirHost(PathBuf);

    impl DictateHost for DirHost {
        fn session_is_live(&self, _key: &SessionSlot) -> bool {
            true
        }

        fn app_support_dir(&self) -> Result<PathBuf, String> {
            Ok(self.0.clone())
        }

        fn emit(&self, _update: DictateUpdate) -> bool {
            true
        }
    }

    /// The preflight config always points the diagnostics store at the
    /// host's app-support dir. A refactor dropping that assignment leaves
    /// the feature shipped-dead with CI green, which is why the assembly is
    /// its own function and this pins it.
    #[test]
    fn preflight_config_points_diagnostics_at_app_support() {
        let settings: DictateSettings = toml::from_str("enabled = true\n").expect("parse");
        let dir = tempfile::tempdir().expect("tempdir");
        let host = DirHost(dir.path().to_path_buf());
        let cfg = preflight_config(&settings, &settings.pin_active(), &host);
        assert_eq!(
            cfg.diagnostics_dir,
            Some(dir.path().join("dictate-diagnostics")),
            "the per-take diagnostics live under the host's app-support base",
        );
        assert_eq!(
            cfg.digest_cache_dir,
            Some(dir.path().join("dictate-digests")),
            "the verified-take cache is what keeps a warm boot off the disk; nothing else arms it",
        );
    }

    /// The events one call logs, by name and level: the level is part of
    /// what the contract says, so a demoted line is not silently accepted.
    #[derive(Clone, Default)]
    struct Logged(std::sync::Arc<std::sync::Mutex<Vec<(tracing::Level, String)>>>);

    struct CollectEventName(String);

    impl tracing::field::Visit for CollectEventName {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "event_name" {
                self.0 = format!("{value:?}").trim_matches('"').to_owned();
            }
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Logged {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut visitor = CollectEventName(String::new());
            event.record(&mut visitor);
            if !visitor.0.is_empty() {
                self.0.lock().expect("capture").push((*event.metadata().level(), visitor.0));
            }
        }
    }

    /// Capture the named events one call logs.
    fn capture<R>(run: impl FnOnce() -> R) -> (R, Vec<(tracing::Level, String)>) {
        use tracing_subscriber::layer::SubscriberExt;
        let logged = Logged::default();
        let subscriber = tracing_subscriber::registry().with(logged.clone());
        let result = tracing::subscriber::with_default(subscriber, run);
        (result, std::mem::take(&mut logged.0.lock().expect("capture")))
    }

    /// A host that cannot resolve an app-support directory turns both stores
    /// off rather than failing preflight - diagnostics never break
    /// dictation, and the absence is what the warning records.
    #[test]
    fn preflight_config_turns_both_stores_off_without_app_support() {
        struct BrokenHost;

        impl DictateHost for BrokenHost {
            fn session_is_live(&self, _key: &SessionSlot) -> bool {
                true
            }

            fn app_support_dir(&self) -> Result<PathBuf, String> {
                Err("no resolvable data/cache/home dir".to_owned())
            }

            fn emit(&self, _update: DictateUpdate) -> bool {
                true
            }
        }

        let settings: DictateSettings = toml::from_str("enabled = true\n").expect("parse");
        let (cfg, logged) =
            capture(|| preflight_config(&settings, &settings.pin_active(), &BrokenHost));
        assert!(
            logged.iter().any(|(level, name)| {
                name == "dictate_state_dir_unresolved" && *level == tracing::Level::WARN
            }),
            "the absence is recorded, not silent: {logged:?}",
        );
        assert!(cfg.diagnostics_dir.is_none(), "per-take diagnostics are off");
        assert!(cfg.digest_cache_dir.is_none(), "and so is the digest record");
    }

    /// Put a live take on the runtime the way a registration does, and hand
    /// back its answer channel so a test can read what the caller gets.
    fn staged_session() -> SessionSlot {
        SessionSlot::new("Test", "proj", "vault-free")
    }

    fn live_take(dictate: &Dictate, owner: &SessionSlot) -> tokio::sync::mpsc::Receiver<bool> {
        let (stop_tx, stop_rx) = tokio::sync::mpsc::channel(1);
        dictate
            .runtime
            .lock()
            .recordings
            .insert(owner.clone(), LiveRecording { stop: stop_tx, sink: None, initiator: None });
        stop_rx
    }

    /// Stop routing reaches the recording that owns the key, and an
    /// unknown key routes nowhere rather than to some other take.
    #[tokio::test]
    async fn a_stop_routes_to_the_take_that_started_on_the_key() {
        let settings: DictateSettings = toml::from_str("enabled = true\n").expect("parse");
        let dictate = Dictate::new(&settings);
        let owner = staged_session();
        let mut stop_rx = live_take(&dictate, &owner);

        handle_dictate_stop(&dictate, &owner, true, None).await;
        assert!(
            matches!(stop_rx.try_recv(), Ok(true)),
            "submit must reach the owning take's channel"
        );

        handle_dictate_stop(&dictate, &SessionSlot::new("Test", "proj", "stranger"), false, None)
            .await;
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
        let settings: DictateSettings = toml::from_str("enabled = true\n").expect("parse");
        let dictate = Dictate::new(&settings);
        let take = staged_session();
        let mut stop_rx = live_take(&dictate, &take);

        move_to_finishing(&dictate, &take);
        {
            let runtime = dictate.runtime.lock();
            assert!(runtime.recordings.is_empty(), "a submitted take no longer holds a live entry");
            assert_eq!(runtime.finishing.len(), 1, "the take is awaiting its transcript");
        }
        handle_dictate_stop(&dictate, &take, false, None).await;
        assert!(
            matches!(stop_rx.try_recv(), Ok(false)),
            "abandon must still reach the take after it finished recording"
        );
    }

    /// A stop the scheduler ordered before its start registered is
    /// parked, not dropped: dropping it orphans the take until the
    /// capture cap releases the microphone.
    #[tokio::test]
    async fn a_stop_before_its_start_is_parked_for_the_registration() {
        let settings: DictateSettings = toml::from_str("enabled = true\n").expect("parse");
        let dictate = Dictate::new(&settings);
        let owner = staged_session();

        handle_dictate_stop(&dictate, &owner, true, None).await;
        assert!(
            matches!(&dictate.runtime.lock().stop_pending, Some((parked, _, _)) if *parked == owner),
            "a stop with nothing to route parks itself for the start"
        );

        // A stop that DOES find a take neither parks nor leaves a stale
        // park behind.
        let mut stop_rx = live_take(&dictate, &owner);
        handle_dictate_stop(&dictate, &owner, true, None).await;
        assert_eq!(stop_rx.recv().await, Some(true));
        assert_eq!(dictate.runtime.lock().stop_pending, None);
    }
}
