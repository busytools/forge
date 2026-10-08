//! Installing a model from the feed, and resolving which model each role
//! runs: the doc's own download link, a size-checked fetch into the models
//! directory, the record of what arrived, and the config-key / runtime-pick
//! / compiled-pin order an active model comes from.
//!
//! **A download is checked against whatever the entry declares: its byte
//! length always, and its digest when it carries one.** The speech feed's
//! docs publish no digest, so those files are checked by length and by
//! whether the engine can load them; the Hub's blobs publish a sha256, and
//! the fetch verifies it once, at install - the record does not keep it,
//! and nothing here may call a file verified.

use std::cell::Cell;
use std::sync::Arc;

use forge_dictate::catalogue::{CatalogueEntry, CatalogueSource, Download, EntryKind};
use forge_dictate::{ModelFacts, ModelSpec, Progress};
use serde::{Deserialize, Serialize};

use crate::bench::BenchState;
use crate::catalogue::PREFERRED_DOWNLOADS;
use crate::dictate::{DictateRole, DictateSettings};
use crate::{DispatchError, SessionUpdate, Workspace};

/// Where an install is, as the page draws it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum InstallState {
    #[default]
    Idle,
    /// `file` is named as soon as the doc says which file this is, so the
    /// page can say what is being fetched rather than a blank bar.
    Downloading {
        file: String,
        got: u64,
        total: u64,
    },
    Failed {
        file: String,
        reason: String,
    },
}

/// Where an activation is, as the page draws it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ActivateState {
    #[default]
    Idle,
    /// A new engine for `file` is building; the old one still serves until
    /// the swap.
    Activating { role: DictateRole, file: String },
    /// The new engine could not load. The previous one still serves, and
    /// the active record still names it.
    Failed { role: DictateRole, file: String, reason: String },
}

/// One model this machine has downloaded from the feed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstalledModel {
    /// The feed's own verb for the model, which is what a config key names.
    pub variant: String,
    pub file: String,
    /// The doc's own URL, kept so a reader can see where the bytes came from.
    pub url: String,
    pub size: u64,
    /// The facts the entry declares, kept so a resolution off this record
    /// can build the same spec the install did without the feed.
    pub facts: ModelFacts,
    /// RFC 3339.
    pub at: String,
}

/// Where a role's model came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// The model one role runs, and where that choice came from.
///
/// Workspace side only: the wire carries `from`/`at` on the page's IN USE
/// rows, which already carry every other field a spec has.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveModel {
    pub role: DictateRole,
    pub spec: ModelSpec,
    pub from: ActiveFrom,
    /// RFC 3339, when a runtime pick chose it.
    pub at: Option<String>,
}

/// The runtime pick as one role records it: the variant, resolved through
/// the installed set, and when the pick was made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveChoice {
    pub variant: String,
    /// RFC 3339.
    pub at: String,
}

/// The `[dictate]` key that pins one role.
fn key_for_role(role: DictateRole) -> &'static str {
    match role {
        DictateRole::Transcribing => "transcribe_model",
        DictateRole::Normalization => "cleanup_model",
    }
}

impl Workspace {
    /// The install's state, for the page's read.
    pub fn dictate_install(&self) -> InstallState {
        self.dictate_install.lock().clone()
    }

    /// Every model downloaded from the feed on this machine, oldest first.
    pub fn installed_models(&self) -> Vec<InstalledModel> {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else { return Vec::new() };
        match crate::store::dictate_models::installed(db) {
            Ok(rows) => rows,
            // A store that cannot be read is a list that would silently read
            // as empty, which looks the same as a machine with nothing on it.
            Err(error) => {
                tracing::warn!(
                    event_name = "dictate_installed_read_failed",
                    %error,
                    "the installed models could not be read; the page shows none"
                );
                Vec::new()
            }
        }
    }

    /// Remove one model this machine downloaded: the file and the record.
    ///
    /// **The file name has to be one this machine recorded**, because it
    /// crosses from a page and joins onto the models directory: a name no
    /// record carries is refused rather than removed, so `../../` and
    /// anything else a caller invents names nothing.
    ///
    /// Refused while a role RUNS the file - the engine holds it loaded - and
    /// named by that role, because the swap that frees it is the caller's
    /// next step rather than this one's. **The record's own runtime pick goes
    /// with it**: a pick left naming a file that is gone refuses the next
    /// boot, and the page that removed the file is the one that can clear it.
    pub(crate) fn uninstall_model(&self, file: &str) -> Result<(), DispatchError> {
        let Some(record) = self.installed_models().into_iter().find(|model| model.file == file)
        else {
            return Err(DispatchError::UninstallRefused {
                reason: "no model this machine downloaded carries that file".to_owned(),
            });
        };
        if let Some((role, _)) =
            self.active_models().iter().find(|(_, model)| model.spec.file == file)
        {
            return Err(DispatchError::ModelInUse { role: *role });
        }
        // The engine holds the file for a RUNNING bench too, and a download
        // in flight is writing it: removing either under the work is a file
        // the bench or the install would then load from nowhere.
        if self.bench_running()
            && matches!(self.dictate_bench(), BenchState::Running { target, .. } if target.file == file)
        {
            return Err(DispatchError::UninstallRefused {
                reason: "the bench is scoring it right now".to_owned(),
            });
        }
        if let InstallState::Downloading { file: writing, .. } = self.dictate_install.lock().clone()
            && writing == file
        {
            return Err(DispatchError::UninstallRefused {
                reason: "the file is being downloaded right now".to_owned(),
            });
        }
        if let Some(dir) = self.config.dictate.models_dir() {
            let path = dir.join(file);
            if let Err(error) = std::fs::remove_file(&path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(%error, file, "the model file was not removed");
                return Err(DispatchError::UninstallRefused { reason: error.to_string() });
            }
        }
        let db = self.db.lock();
        if let Some(db) = db.as_ref() {
            if let Err(error) = crate::store::dictate_models::remove_installed(db, file) {
                tracing::warn!(
                    event_name = "dictate_uninstall_record_failed",
                    %error,
                    file,
                    "the model was removed from disk but its record was not; the page still lists it"
                );
            }
            for role in [DictateRole::Transcribing, DictateRole::Normalization] {
                let picked = crate::store::dictate_models::active(db, role)
                    .ok()
                    .flatten()
                    .is_some_and(|choice| choice.variant == record.variant);
                if picked
                    && let Err(error) = crate::store::dictate_models::clear_active(db, role)
                {
                    tracing::warn!(
                        event_name = "dictate_uninstall_pick_clear_failed",
                        %error,
                        role = crate::store::dictate_models::role_key(role),
                        "the removal left a runtime pick naming the model that is gone"
                    );
                }
            }
        }
        Ok(())
    }

    /// Install one feed variant: fetch its doc, take the quant the row
    /// draws, and download that file into the models directory.
    ///
    /// The outcome rides [`SessionUpdate::DictateModelsChanged`], the same
    /// way a catalogue check's does.
    pub(crate) fn start_install(self: &Arc<Self>, variant: String) -> Result<(), DispatchError> {
        if !self.config.dictate.enabled {
            return Err(DispatchError::DictateOff);
        }
        if self.bench_running() {
            return Err(DispatchError::BenchRunning);
        }
        {
            let mut state = self.dictate_install.lock();
            if matches!(*state, InstallState::Downloading { .. }) {
                return Err(DispatchError::Installing);
            }
            *state = InstallState::Downloading { file: String::new(), got: 0, total: 0 };
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let runner = Arc::clone(&this);
            runner.run_install(variant).await;
            this.push_models();
        });
        Ok(())
    }

    /// Take the install no further, naming the step that stopped.
    fn fail_install(&self, file: String, reason: String) {
        *self.dictate_install.lock() = InstallState::Failed { file, reason };
    }

    fn note_download(&self, file: &str, got: u64, total: u64) {
        *self.dictate_install.lock() =
            InstallState::Downloading { file: file.to_owned(), got, total };
    }

    /// Name the file while the doc is still being read, so the page's bar
    /// has something to say from the first tick.
    fn set_install_file(&self, file: &str, total: u64) {
        self.note_download(file, 0, total);
    }

    /// Record a downloaded model, answering whether the store took it.
    ///
    /// `false` is a store that could not write, which is the caller's to act
    /// on: a file nothing recorded is a file the page can neither show nor
    /// remove, so the install path takes the bytes back rather than leaving
    /// one behind.
    fn record_installed(&self, model: &InstalledModel) -> bool {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else {
            tracing::warn!(
                event_name = "dictate_install_record_failed",
                file = %model.file,
                "no store is open; the download was not recorded"
            );
            return false;
        };
        match crate::store::dictate_models::record_installed(db, model) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(
                    event_name = "dictate_install_record_failed",
                    %error,
                    file = %model.file,
                    "the download was not recorded; the page cannot list or remove it"
                );
                false
            }
        }
    }

    /// The page's re-read, pushed the way a landed check pushes one.
    pub(crate) fn push_models(&self) {
        let models = self.dictate_models();
        let _ = self.update_sender().send(SessionUpdate::DictateModelsChanged { models });
    }

    /// The feed source and the entry a variant names, when both are known.
    fn variant_for_install(&self, variant: &str) -> Option<(CatalogueSource, CatalogueEntry)> {
        let source = self.catalogue_source();
        let state = self.dictate_catalogue.lock();
        let entry = state
            .catalogue
            .as_ref()?
            .entries
            .iter()
            .find(|entry| entry.variant == variant)?
            .clone();
        Some((source, entry))
    }

    /// Fetch, check and record one variant. Every failure lands as an
    /// `InstallState::Failed` whose reason is its own, so the page says
    /// which step stopped rather than that something did.
    async fn run_install(self: Arc<Self>, variant: String) {
        let Some((source, entry)) = self.variant_for_install(&variant) else {
            return self.fail_install(String::new(), format!("no catalogue entry names {variant}"));
        };
        let Some(download) = preferred_download(&entry).cloned() else {
            return self.fail_install(
                String::new(),
                format!("{variant} ships no quantisation this machine would run"),
            );
        };
        let spec = match spec_for_entry(source, &entry, &download).await {
            Ok(spec) => spec,
            Err(reason) => return self.fail_install(download.filename, reason),
        };

        // The base is the `[dictate]` settings, with the candidate in the
        // ASR slot and no normalizer: `prepare` fetches what the config
        // names, and this config names one file.
        let Some(dir) = self.config.dictate.models_dir() else {
            return self
                .fail_install(download.filename, "no models directory is configured".to_owned());
        };
        let mut cfg = self.config.dictate.to_config();
        cfg.models_dir = Some(dir);
        cfg.asr_model = spec.clone();
        cfg.normalizer = None;

        self.set_install_file(&download.filename, download.size_bytes);
        // The file is named: the page's line can say what is being fetched
        // before the first byte moves.
        self.push_models();
        let this = Arc::clone(&self);
        let file = download.filename.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            // The push rides the whole-percent change: a tick per progress
            // event would re-encode the page's snapshot thousands of times
            // over a 279 MB file, and a percent is what a reader sees move.
            // The push itself matters as much as the throttle: a state the
            // server records but never sends is a page that sits on `0 of 0`
            // until the install is over.
            let last = Cell::new(u64::MAX);
            forge_dictate::prepare(&cfg, |progress| {
                if let Progress::Downloading { downloaded, total, .. } = progress {
                    let percent = downloaded.saturating_mul(100).checked_div(total).unwrap_or(0);
                    if percent != last.get() {
                        last.set(percent);
                        this.note_download(&file, downloaded, total);
                        this.push_models();
                    }
                }
                std::ops::ControlFlow::Continue(())
            })
        })
        .await;

        match prepared {
            Ok(Ok(())) => {
                let recorded = self.record_installed(&InstalledModel {
                    variant,
                    file: spec.file.clone(),
                    url: spec.url,
                    size: spec.size,
                    facts: spec.facts,
                    at: rfc3339_now(),
                });
                // **A file nothing recorded goes back.** The page lists and
                // removes by record, so an unrecorded file is one no control
                // can reach - the install takes its bytes back rather than
                // leaving litter behind it.
                if !recorded {
                    if let Some(dir) = self.config.dictate.models_dir() {
                        let _ = std::fs::remove_file(dir.join(&spec.file));
                    }
                    self.fail_install(spec.file, "the download could not be recorded".to_owned());
                    return;
                }
                *self.dictate_install.lock() = InstallState::Idle;
            }
            Ok(Err(error)) => self.fail_install(download.filename, error.to_string()),
            Err(join) => self.fail_install(download.filename, join.to_string()),
        }
    }
}

/// Build and load one engine off the runtime thread, answering it or the
/// reason it could not serve.
///
/// Building before swapping is the order: a model that will not load
/// leaves the previous engine serving rather than the role with nothing.
async fn build_engine(cfg: forge_dictate::Config) -> Result<Arc<forge_dictate::Engine>, String> {
    tokio::task::spawn_blocking(move || {
        let engine = forge_dictate::Engine::new(cfg)?;
        engine.wait_ready()?;
        Ok::<Arc<forge_dictate::Engine>, forge_dictate::Error>(engine)
    })
    .await
    .map_err(|join| join.to_string())?
    .map_err(|error| error.to_string())
}

/// The spec one feed entry describes for the quant a row draws: where the
/// file is, the entry's own byte length, the digest where its host publishes
/// one, and the facts both carry.
///
/// The Hub's entries carry the URL and the digest themselves; the speech
/// feed's carry neither, so its documents are read for the link - which is
/// also where that feed's files' only verification (the byte length) comes
/// from.
async fn spec_for_entry(
    source: CatalogueSource,
    entry: &CatalogueEntry,
    download: &Download,
) -> Result<ModelSpec, String> {
    let url = if let Some(url) = download.url.clone() {
        url
    } else {
        let variant = entry.variant.clone();
        let repo = entry.published_repo.clone();
        let links = tokio::task::spawn_blocking(move || {
            forge_dictate::catalogue::download_links(&source, &variant, repo.as_deref())
        })
        .await
        .map_err(|join| join.to_string())?
        .map_err(|error| error.to_string())?;
        match links.into_iter().find(|(file, _)| file == &download.filename) {
            Some((_, url)) => url,
            None => {
                return Err(format!(
                    "the feed's documents carry no download for {}",
                    download.filename
                ));
            }
        }
    };
    Ok(forge_dictate::spec_for_download(
        &download.filename,
        &url,
        download.size_bytes,
        download.sha256.clone(),
        ModelFacts {
            quant: Some(download.quant.clone()),
            params: entry.params,
            license: entry.license.as_ref().map(|license| license.display.clone()),
            // What loads the file: the speech models run on transcribe.cpp
            // and a normalizer is llama.cpp's, which is the entry's own kind
            // saying so rather than a guess about the feed.
            runtime: Some(match entry.kind {
                EntryKind::Asr => "transcribe.cpp".to_owned(),
                EntryKind::Normalizer => "llama.cpp".to_owned(),
            }),
        },
    ))
}

/// The download the row draws: the preferred quant chain, most wanted first.
fn preferred_download(entry: &CatalogueEntry) -> Option<&Download> {
    PREFERRED_DOWNLOADS
        .iter()
        .find_map(|quant| entry.downloads.iter().find(|download| &download.quant == quant))
}

impl Workspace {
    /// The models the roles run, as the preflight resolved them: the
    /// compiled pins until that resolution lands, then whatever the config
    /// key, the runtime pick or the pin answered, per role.
    pub fn active_models(&self) -> Vec<(DictateRole, ActiveModel)> {
        self.dictate.active.lock().clone()
    }

    /// Resolve every role's model, fresh: the config key when set, else
    /// the runtime pick, else the compiled pin.
    ///
    /// The key's variant resolves through the installed record first, a
    /// local read; a variant that is not on disk reaches the feed's doc
    /// table, which is the download source and the only network call here.
    /// A key that resolves through neither is an error naming the key,
    /// which the preflight renders as the reason forge stops.
    pub(crate) async fn resolve_active(
        &self,
        settings: &DictateSettings,
    ) -> Result<Vec<(DictateRole, ActiveModel)>, String> {
        let mut resolved = Vec::new();
        for (role, pin) in settings.model_specs() {
            let key = key_for_role(role);
            let configured = match role {
                DictateRole::Transcribing => settings.transcribe_model.as_deref(),
                DictateRole::Normalization => settings.cleanup_model.as_deref(),
            };
            // A config key wins over everything, and it never clears the
            // record: removing the key returns the role to the last
            // runtime pick.
            if let Some(variant) = configured {
                let spec = self.resolve_variant(key, variant).await?;
                resolved.push((
                    role,
                    ActiveModel {
                        role,
                        spec,
                        from: ActiveFrom::Config {
                            key: key.to_owned(),
                            variant: variant.to_owned(),
                        },
                        at: None,
                    },
                ));
                continue;
            }
            if let Some(choice) = self.active_choice(role) {
                let installed = self
                    .installed_models()
                    .into_iter()
                    .find(|model| model.variant == choice.variant);
                // A pick whose record is gone falls to the pin rather than
                // refusing the boot: the pick is forge's own bookkeeping, so
                // a removal that stranded one must not take the launchpad
                // with it. The removal clears the pick it strands.
                let Some(installed) = installed else {
                    tracing::warn!(
                        event_name = "dictate_pick_without_a_record",
                        role = crate::store::dictate_models::role_key(role),
                        variant = %choice.variant,
                        "the runtime pick names a model no record carries; the compiled pin runs this role"
                    );
                    resolved.push((
                        role,
                        ActiveModel { role, spec: pin, from: ActiveFrom::Pin, at: None },
                    ));
                    continue;
                };
                resolved.push((
                    role,
                    ActiveModel {
                        role,
                        spec: ModelSpec {
                            file: installed.file,
                            url: installed.url,
                            size: installed.size,
                            sha256: None,
                            facts: installed.facts,
                        },
                        from: ActiveFrom::Installed { variant: choice.variant },
                        at: Some(choice.at),
                    },
                ));
                continue;
            }
            resolved.push((role, ActiveModel { role, spec: pin, from: ActiveFrom::Pin, at: None }));
        }
        Ok(resolved)
    }

    /// The runtime pick recorded for one role, when the store answers one.
    ///
    /// A failed read reads as no pick - the role falls to the pin - and it
    /// says so, because a corrupt row silently reverting the pick would look
    /// like a pick that never happened.
    fn active_choice(&self, role: DictateRole) -> Option<ActiveChoice> {
        let db = self.db.lock();
        let db = db.as_ref()?;
        match crate::store::dictate_models::active(db, role) {
            Ok(choice) => choice,
            Err(error) => {
                tracing::warn!(
                    event_name = "dictate_active_read_failed",
                    %error,
                    role = crate::store::dictate_models::role_key(role),
                    "the runtime pick could not be read; the role falls to the compiled pin"
                );
                None
            }
        }
    }

    /// The model a config key names: the installed record, else the feed's
    /// doc table, else nothing.
    async fn resolve_variant(&self, key: &str, variant: &str) -> Result<ModelSpec, String> {
        if let Some(installed) =
            self.installed_models().into_iter().find(|model| model.variant == variant)
        {
            return Ok(ModelSpec {
                file: installed.file,
                url: installed.url,
                size: installed.size,
                sha256: None,
                facts: installed.facts,
            });
        }
        let Some(entry) = self.catalogue_entry_for(variant).await else {
            return Err(format!(
                "[dictate] {key} names {variant}, which is neither an installed model nor a \
                 catalogue entry"
            ));
        };
        let Some(download) = preferred_download(&entry).cloned() else {
            return Err(format!(
                "[dictate] {key} names {variant}, which ships no quantisation this machine would \
                 run"
            ));
        };
        spec_for_entry(self.catalogue_source(), &entry, &download)
            .await
            .map_err(|reason| format!("[dictate] {key} names {variant}: {reason}"))
    }

    /// The feed's entry for one variant: the loaded feed, or one catalogue
    /// fetch when the loaded feed does not carry it.
    ///
    /// The fetch is what keeps a config key working on a fresh machine,
    /// whose local store and cache both predate the variant.
    async fn catalogue_entry_for(&self, variant: &str) -> Option<CatalogueEntry> {
        let loaded = {
            let state = self.dictate_catalogue.lock();
            state.catalogue.as_ref().and_then(|catalogue| {
                catalogue.entries.iter().find(|entry| entry.variant == variant).cloned()
            })
        };
        if loaded.is_some() {
            return loaded;
        }
        let fetched = self.fetch_catalogue_once().await.ok()?;
        fetched.entries.into_iter().find(|entry| entry.variant == variant)
    }

    /// Record a config-resolved model the preflight has just fetched, so
    /// the page's installed set and the next boot's resolution both see it.
    ///
    /// Only the config leg reaches here: a runtime pick was installed
    /// before it could be picked, and the compiled pins are not recorded,
    /// because nothing here names their variant without the feed.
    pub(crate) fn record_resolved_installs(&self, resolved: &[(DictateRole, ActiveModel)]) {
        for (_, model) in resolved {
            let ActiveFrom::Config { variant, .. } = &model.from else { continue };
            if self.installed_models().iter().any(|installed| &installed.variant == variant) {
                continue;
            }
            self.record_installed(&InstalledModel {
                variant: variant.clone(),
                file: model.spec.file.clone(),
                url: model.spec.url.clone(),
                size: model.spec.size,
                facts: model.spec.facts.clone(),
                at: rfc3339_now(),
            });
        }
    }
}

impl Workspace {
    /// The activation's state, for the page's read.
    pub fn dictate_activate(&self) -> ActivateState {
        self.dictate_activate.lock().clone()
    }

    /// Make one installed model the role's active model on the running
    /// forge: build the new engine while the old still serves, swap, then
    /// drop the old.
    ///
    /// A role `forge.toml` pins is refused by name - the page can still
    /// download and bench it, but the runtime cannot move it - and a live
    /// take refuses the swap rather than being dropped under.
    pub(crate) fn start_activate(
        self: &Arc<Self>,
        role: DictateRole,
        file: String,
    ) -> Result<(), DispatchError> {
        if !self.config.dictate.enabled {
            return Err(DispatchError::DictateOff);
        }
        if let Some(key) = self.pinning_key(role) {
            return Err(DispatchError::PinnedRole { key });
        }
        if let Some(holder) = self.dictate_runtime.lock().live_holder() {
            return Err(DispatchError::TakeLive { holder });
        }
        if self.bench_running() {
            return Err(DispatchError::BenchRunning);
        }
        {
            let mut state = self.dictate_activate.lock();
            if matches!(*state, ActivateState::Activating { .. }) {
                return Err(DispatchError::Activating);
            }
            *state = ActivateState::Activating { role, file: file.clone() };
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let runner = Arc::clone(&this);
            runner.run_activate(role, file).await;
            this.push_models();
        });
        Ok(())
    }

    /// Return one role to its `[dictate]` key or compiled default: clear
    /// the runtime pick and swap the engine back, in activation's own
    /// build-then-swap order.
    pub(crate) fn start_deactivate(
        self: &Arc<Self>,
        role: DictateRole,
    ) -> Result<(), DispatchError> {
        if !self.config.dictate.enabled {
            return Err(DispatchError::DictateOff);
        }
        if let Some(key) = self.pinning_key(role) {
            return Err(DispatchError::PinnedRole { key });
        }
        if let Some(holder) = self.dictate_runtime.lock().live_holder() {
            return Err(DispatchError::TakeLive { holder });
        }
        if self.bench_running() {
            return Err(DispatchError::BenchRunning);
        }
        let Some(pin) = self.pin_for(role) else {
            return Err(DispatchError::DictateOff);
        };
        {
            let mut state = self.dictate_activate.lock();
            if matches!(*state, ActivateState::Activating { .. }) {
                return Err(DispatchError::Activating);
            }
            *state = ActivateState::Activating { role, file: pin.file.clone() };
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let runner = Arc::clone(&this);
            runner.run_deactivate(role, pin).await;
            this.push_models();
        });
        Ok(())
    }

    /// The `[dictate]` key that pins one role, when one does.
    fn pinning_key(&self, role: DictateRole) -> Option<String> {
        let settings = &self.config.dictate;
        let pinned = match role {
            DictateRole::Transcribing => settings.transcribe_model.as_ref(),
            DictateRole::Normalization => settings.cleanup_model.as_ref(),
        };
        pinned.map(|_| key_for_role(role).to_owned())
    }

    /// The compiled default one role runs, when it has one: the cleanup
    /// role has none while `normalizer` is off.
    fn pin_for(&self, role: DictateRole) -> Option<ModelSpec> {
        self.config
            .dictate
            .model_specs()
            .into_iter()
            .find(|(spec_role, _)| *spec_role == role)
            .map(|(_, spec)| spec)
    }

    /// Load one installed model and swap the role onto it.
    async fn run_activate(self: Arc<Self>, role: DictateRole, file: String) {
        let Some(installed) = self.installed_models().into_iter().find(|model| model.file == file)
        else {
            return self.fail_activate(role, file, "no installed model has that file".to_owned());
        };
        let variant = installed.variant.clone();
        let spec = ModelSpec {
            file: installed.file,
            url: installed.url,
            size: installed.size,
            sha256: None,
            facts: installed.facts,
        };
        match self
            .swap_role_to(role, spec, ActiveFrom::Installed { variant: variant.clone() })
            .await
        {
            Ok(at) => {
                self.record_pick(role, &variant, &at);
                *self.dictate_activate.lock() = ActivateState::Idle;
            }
            Err(reason) => self.fail_activate(role, file, reason),
        }
    }

    /// Swap the role back to its compiled default and drop the pick.
    async fn run_deactivate(self: Arc<Self>, role: DictateRole, pin: ModelSpec) {
        let file = pin.file.clone();
        match self.swap_role_to(role, pin, ActiveFrom::Pin).await {
            Ok(_) => {
                self.clear_pick(role);
                *self.dictate_activate.lock() = ActivateState::Idle;
            }
            Err(reason) => self.fail_activate(role, file, reason),
        }
    }

    /// Build the engine the role's new model needs while the old one still
    /// serves, swap the `Arc`, then drop the old - never drop first, which
    /// would leave the role with no engine at all if the new load fails.
    ///
    /// Answers the RFC 3339 stamp the pick is recorded with, or the reason
    /// the load failed; a failure changes nothing.
    async fn swap_role_to(
        &self,
        role: DictateRole,
        spec: ModelSpec,
        from: ActiveFrom,
    ) -> Result<String, String> {
        let current = self.active_models();
        if current.iter().any(|(r, model)| *r == role && model.spec.file == spec.file) {
            // The role already runs this file: nothing to build, and a
            // rebuild would cost a second model load for no change.
            return Ok(rfc3339_now());
        }
        let stamp = rfc3339_now();
        // `at` is the runtime pick's own stamp; a pin or a config leg is not
        // a pick, so it carries none however it got there.
        let at = match from {
            ActiveFrom::Installed { .. } => Some(stamp.clone()),
            ActiveFrom::Config { .. } | ActiveFrom::Pin => None,
        };
        let next: Vec<(DictateRole, ActiveModel)> = current
            .into_iter()
            .map(|(r, model)| {
                if r == role {
                    (
                        role,
                        ActiveModel {
                            role,
                            spec: spec.clone(),
                            from: from.clone(),
                            at: at.clone(),
                        },
                    )
                } else {
                    (r, model)
                }
            })
            .collect();

        let cfg = crate::dictate::preflight_config(&self.config.dictate, &next);
        let engine = build_engine(cfg).await?;
        let previous = self.dictate.engine.lock().replace(engine);
        drop(previous);

        self.dictate.set_active(&next);
        self.dictate.mark_all(&crate::dictate::DictateModelState::Ready);
        Ok(stamp)
    }

    fn fail_activate(&self, role: DictateRole, file: String, reason: String) {
        *self.dictate_activate.lock() = ActivateState::Failed { role, file, reason };
    }

    /// Record one role's runtime pick, so the next boot answers it.
    fn record_pick(&self, role: DictateRole, variant: &str, at: &str) {
        let db = self.db.lock();
        if let Some(db) = db.as_ref()
            && let Err(error) = crate::store::dictate_models::record_active(
                db,
                role,
                &ActiveChoice { variant: variant.to_owned(), at: at.to_owned() },
            )
        {
            tracing::warn!(
                event_name = "dictate_active_record_failed",
                %error,
                %variant,
                "the runtime pick was not recorded; the next boot answers the previous model"
            );
        }
    }

    /// Drop one role's runtime pick, so the role answers its config key or
    /// its compiled pin again.
    fn clear_pick(&self, role: DictateRole) {
        let db = self.db.lock();
        if let Some(db) = db.as_ref()
            && let Err(error) = crate::store::dictate_models::clear_active(db, role)
        {
            tracing::warn!(
                event_name = "dictate_active_clear_failed",
                %error,
                "the runtime pick was not cleared; the next boot answers it again"
            );
        }
    }
}

/// A `SystemTime` as RFC 3339, the stamp a result is comparable by.
pub(crate) fn rfc3339_now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests_install {
    use super::*;
    use crate::catalogue::DictateModelsSnapshot;
    use crate::catalogue::tests_catalogue_view::{
        enabled_stub, entry_under, serve, serve_with, source,
    };
    use crate::{Command, DispatchError, SessionUpdate, Workspace};
    use forge_dictate::catalogue::Catalogue;

    /// One fetched feed carrying the given entries, as the install path
    /// reads it: the variant lookup goes through this state.
    fn catalogue_of(entries: Vec<forge_dictate::catalogue::CatalogueEntry>) -> Catalogue {
        Catalogue {
            fetched_at: "2026-10-06T00:00:00Z".to_owned(),
            release: None,
            entries,
            skipped: 0,
        }
    }

    /// A doc body shaped like the feed's own: the marked table carrying
    /// one quant row, its link pointing back at the loopback that serves it.
    fn doc_body(base: &str, variant: &str, file: &str) -> Vec<u8> {
        format!(
            "# {variant}\n\n<!-- catalog:downloads -->\n\
             | Quantization | Download | Size |\n\
             | --- | --- | ---: |\n\
             | Q4_K_M | [{file}]({base}/weights/{file}) | 6 B |\n\
             <!-- /catalog -->\n"
        )
        .into_bytes()
    }

    /// Everything a store-backed install test holds alive: the stub, its
    /// update feed, and the directories the models and the record land in.
    /// The plain `enabled_stub` keeps `db` at `None`, so recording is
    /// silently skipped there and a read-back test would pass vacuously.
    struct Fixture {
        ws: Arc<Workspace>,
        updates: tokio::sync::mpsc::UnboundedReceiver<SessionUpdate>,
        _models: tempfile::TempDir,
        _store: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        fixture_with(None, None)
    }

    /// [`fixture`] with the `[dictate]` model keys a resolution test needs.
    fn fixture_with(transcribe_model: Option<&str>, cleanup_model: Option<&str>) -> Fixture {
        fixture_built(|config| {
            config.dictate.transcribe_model = transcribe_model.map(str::to_owned);
            config.dictate.cleanup_model = cleanup_model.map(str::to_owned);
        })
    }

    /// [`fixture`] with anything else a test needs set on the config before
    /// the workspace is built.
    fn fixture_built(configure: impl FnOnce(&mut crate::config::LoadedConfig)) -> Fixture {
        let config_dir = tempfile::tempdir().expect("a config dir");
        let models = tempfile::tempdir().expect("a models dir");
        let store = tempfile::tempdir().expect("a store dir");
        let mut config = crate::config::LoadedConfig::empty_for_test();
        config.dictate.enabled = true;
        config.dictate.models_dir = Some(models.path().to_string_lossy().into_owned());
        configure(&mut config);
        let (ws, updates) =
            Workspace::testing_stub_with_config(config_dir.path().to_path_buf(), config)
                .expect("the stub builds");
        ws.install_db_for_test(
            crate::store::Db::open(&store.path().join("db.redb")).expect("the store opens"),
        );
        Fixture { ws, updates, _models: models, _store: store }
    }

    /// The `[dictate]` settings a resolution test resolves against.
    fn settings_of(ws: &Workspace) -> DictateSettings {
        ws.config.dictate.clone()
    }

    /// Put one variant in the installed set, as an install of it would.
    fn record_installed_model(ws: &Workspace, variant: &str) {
        ws.record_installed(&InstalledModel {
            variant: variant.to_owned(),
            file: format!("{variant}-Q4_K_M.gguf"),
            url: format!("https://weights.invalid/{variant}-Q4_K_M.gguf"),
            size: 6,
            facts: ModelFacts { quant: Some("Q4_K_M".to_owned()), ..ModelFacts::default() },
            at: "2026-10-06T00:00:00Z".to_owned(),
        });
    }

    /// **A model is removed by file name, and refused while a role runs it.**
    /// A sweep leaves the candidates it scored and did not adopt behind, and
    /// the file on disk is the litter - but the engine holds the file a role
    /// runs, so that one goes through the role first.
    #[tokio::test]
    async fn a_model_is_removed_by_file_and_refused_while_a_role_runs_it() {
        let Fixture { ws, mut updates, _models, .. } = fixture();
        record_installed_model(&ws, "spare");
        record_installed_model(&ws, "ruling");

        // The file on disk is what the removal is about.
        let dir = ws.config.dictate.models_dir().expect("the fixture sets one");
        std::fs::write(dir.join("spare-Q4_K_M.gguf"), b"six!!!").unwrap();

        // The role runs one of them: its swap is the caller's next step, and
        // the refusal names the label.
        ws.dictate.set_active(&[(
            DictateRole::Normalization,
            crate::install::ActiveModel {
                role: DictateRole::Normalization,
                spec: forge_dictate::ModelSpec {
                    file: "ruling-Q4_K_M.gguf".to_owned(),
                    ..forge_dictate::ModelSpec::cohere_transcribe_q4_k_m()
                },
                from: crate::install::ActiveFrom::Pin,
                at: None,
            },
        )]);
        let err = ws
            .dispatch(Command::DictateUninstall { file: "ruling-Q4_K_M.gguf".to_owned() })
            .expect_err("a role is running it");
        assert!(
            matches!(&err, DispatchError::ModelInUse { role } if *role == DictateRole::Normalization),
            "got: {err:?}"
        );

        ws.dispatch(Command::DictateUninstall { file: "spare-Q4_K_M.gguf".to_owned() })
            .expect("nothing runs it");
        assert!(!dir.join("spare-Q4_K_M.gguf").exists(), "the file went with the record");
        let rows = ws.installed_models();
        assert_eq!(rows.len(), 1, "the record went with the file");
        assert_eq!(rows[0].file, "ruling-Q4_K_M.gguf");
        let last = {
            let mut last = None;
            while let Ok(update) = updates.try_recv() {
                if let SessionUpdate::DictateModelsChanged { models } = update {
                    last = Some(models);
                }
            }
            last
        };
        assert_eq!(
            last.map(|models| models.installed.len()),
            Some(1),
            "and the push carries the read without it"
        );
    }

    /// **A file name no record carries is refused, not joined.** The name
    /// crosses from a page, so `../..` and anything else a caller invents
    /// must name nothing rather than remove something outside the models
    /// directory - the same rule `delete_read_aloud` keeps for a take.
    #[tokio::test]
    async fn a_removal_names_a_recorded_file_or_is_refused() {
        let Fixture { ws, .. } = fixture();
        record_installed_model(&ws, "spare");

        for invented in ["../../Documents/notes.md", "ruling-Q4_K_M.gguf"] {
            let err = ws
                .dispatch(Command::DictateUninstall { file: invented.to_owned() })
                .expect_err("nothing recorded carries these bytes");
            assert!(
                matches!(&err, DispatchError::UninstallRefused { .. }),
                "got: {err:?}"
            );
        }
        assert_eq!(ws.installed_models().len(), 1, "and nothing went with the refusal");
    }

    /// The belt for a pick that is already stranded: one naming a variant no
    /// record carries resolves to the pin with a warning rather than refusing
    /// the boot whole.
    #[tokio::test]
    async fn a_pick_without_a_record_resolves_to_the_pin() {
        let Fixture { ws, .. } = fixture();
        record_active(&ws, DictateRole::Transcribing, "gone");

        let settings = settings_of(&ws);
        let resolved = ws.resolve_active(&settings).await.expect("a stranded pick still boots");
        let (role, model) =
            resolved.iter().find(|(role, _)| *role == DictateRole::Transcribing).expect("resolved");
        assert_eq!(*role, DictateRole::Transcribing);
        assert!(
            matches!(model.from, crate::install::ActiveFrom::Pin),
            "got: {:?}",
            model.from
        );
    }

    /// **A removal clears the runtime pick it strands.** A pick left naming a
    /// file that is gone refused the next boot - forge never left the
    /// launchpad and nothing on the page could fix it - so the removal is
    /// where the invariant is kept.
    #[tokio::test]
    async fn a_removal_clears_the_pick_that_named_it() {
        let Fixture { ws, .. } = fixture();
        record_installed_model(&ws, "spare");
        record_active(&ws, DictateRole::Normalization, "spare");

        ws.dispatch(Command::DictateUninstall { file: "spare-Q4_K_M.gguf".to_owned() })
            .expect("nothing runs it");

        let active = {
            let db = ws.db.lock();
            crate::store::dictate_models::active(
                db.as_ref().expect("the fixture installs a store"),
                DictateRole::Normalization,
            )
            .expect("the store answers")
        };
        assert!(active.is_none(), "the pick went with the record it named");
    }

    /// Record one role's runtime pick, as an activation of it would.
    fn record_active(ws: &Workspace, role: DictateRole, variant: &str) {
        let db = ws.db.lock();
        crate::store::dictate_models::record_active(
            db.as_ref().expect("the fixture installs a store"),
            role,
            &ActiveChoice { variant: variant.to_owned(), at: "2026-10-06T01:00:00Z".to_owned() },
        )
        .expect("the pick records");
    }

    /// One role's resolved model.
    fn role_model(resolved: &[(DictateRole, ActiveModel)], role: DictateRole) -> &ActiveModel {
        &resolved.iter().find(|(resolved, _)| *resolved == role).expect("the role resolves").1
    }

    /// The config key wins over the runtime pick and the compiled pin:
    /// `forge.toml` names the model, and the runtime cannot move it.
    #[tokio::test]
    async fn a_config_key_beats_the_record_and_the_pin() {
        let fixture = fixture_with(Some("keyed"), None);
        record_installed_model(&fixture.ws, "keyed");
        record_installed_model(&fixture.ws, "picked");
        record_active(&fixture.ws, DictateRole::Transcribing, "picked");
        let settings = settings_of(&fixture.ws);

        let resolved = fixture.ws.resolve_active(&settings).await.expect("the key resolves");

        let transcribing = role_model(&resolved, DictateRole::Transcribing);
        assert_eq!(transcribing.spec.file, "keyed-Q4_K_M.gguf", "the key's variant, not the pick");
        assert_eq!(
            transcribing.from,
            ActiveFrom::Config { key: "transcribe_model".to_owned(), variant: "keyed".to_owned() },
            "and the page can name the key that holds the role"
        );
    }

    /// Without a key the runtime pick wins over the compiled pin, and it
    /// answers with the facts the install recorded.
    #[tokio::test]
    async fn without_a_key_the_record_beats_the_pin() {
        let fixture = fixture_with(None, None);
        record_installed_model(&fixture.ws, "picked");
        record_active(&fixture.ws, DictateRole::Transcribing, "picked");
        let settings = settings_of(&fixture.ws);

        let resolved = fixture.ws.resolve_active(&settings).await.expect("the pick resolves");

        let transcribing = role_model(&resolved, DictateRole::Transcribing);
        assert_eq!(transcribing.spec.file, "picked-Q4_K_M.gguf");
        assert_eq!(transcribing.spec.facts.quant.as_deref(), Some("Q4_K_M"));
        assert_eq!(transcribing.from, ActiveFrom::Installed { variant: "picked".to_owned() });
        assert_eq!(transcribing.at.as_deref(), Some("2026-10-06T01:00:00Z"));
    }

    /// With neither, the compiled pin stands, for both roles.
    #[tokio::test]
    async fn without_a_key_or_a_record_the_pin_stands() {
        let fixture = fixture_with(None, None);
        let settings = settings_of(&fixture.ws);

        let resolved = fixture.ws.resolve_active(&settings).await.expect("the pins resolve");

        let transcribing = role_model(&resolved, DictateRole::Transcribing);
        assert_eq!(transcribing.spec.file, "cohere-transcribe-03-2026-Q4_K_M.gguf");
        assert_eq!(transcribing.from, ActiveFrom::Pin);
        assert_eq!(transcribing.at, None);
        assert_eq!(role_model(&resolved, DictateRole::Normalization).spec.file, "s1-mini-f16.gguf");
    }

    /// Setting a key never clears the pick: removing the key returns the
    /// role to the last runtime choice, not to the compiled pin.
    #[tokio::test]
    async fn setting_a_key_leaves_the_record_alone_so_unpinning_returns_to_it() {
        let fixture = fixture_with(Some("keyed"), None);
        record_installed_model(&fixture.ws, "keyed");
        record_installed_model(&fixture.ws, "picked");
        record_active(&fixture.ws, DictateRole::Transcribing, "picked");
        let settings = settings_of(&fixture.ws);

        let pinned = fixture.ws.resolve_active(&settings).await.expect("the key resolves");
        assert_eq!(role_model(&pinned, DictateRole::Transcribing).spec.file, "keyed-Q4_K_M.gguf");

        let unpinned = DictateSettings { transcribe_model: None, ..settings };
        let resolved = fixture.ws.resolve_active(&unpinned).await.expect("the pick resolves");

        let transcribing = role_model(&resolved, DictateRole::Transcribing);
        assert_eq!(
            transcribing.spec.file, "picked-Q4_K_M.gguf",
            "unpinning returns the role to the last runtime pick, not the pin"
        );
        assert_eq!(transcribing.from, ActiveFrom::Installed { variant: "picked".to_owned() });
    }

    /// A key naming a variant the installed set and the feed both lack is
    /// the one resolution failure: the error names the key and the
    /// variant, and the preflight renders it as the reason forge stops.
    #[tokio::test]
    async fn a_key_naming_a_variant_that_resolves_to_nothing_fails_boot_by_name() {
        let fixture = fixture_with(Some("ghost"), None);
        let base = serve(vec![("/catalog", 200, b"[]".to_vec())]);
        *fixture.ws.test_catalogue_source.lock() = Some(source(&base));
        let settings = settings_of(&fixture.ws);

        let err =
            fixture.ws.resolve_active(&settings).await.expect_err("nothing names the variant");

        assert!(
            err.contains("[dictate] transcribe_model"),
            "the error names the key that cannot be answered, got: {err}"
        );
        assert!(err.contains("ghost"), "and the variant it names, got: {err}");
    }

    /// A config-resolved variant the preflight has fetched is recorded as
    /// installed - the record the page's installed set and the next boot's
    /// resolution read - and recording it again changes nothing.
    #[tokio::test]
    async fn a_key_resolved_variant_is_recorded_as_installed_once() {
        let fixture = fixture_with(None, None);
        let resolved = vec![(
            DictateRole::Transcribing,
            ActiveModel {
                role: DictateRole::Transcribing,
                spec: forge_dictate::spec_for_download(
                    "granite-Q4_K_M.gguf",
                    "https://weights.invalid/granite-Q4_K_M.gguf",
                    6,
                    None,
                    ModelFacts { quant: Some("Q4_K_M".to_owned()), ..ModelFacts::default() },
                ),
                from: ActiveFrom::Config {
                    key: "transcribe_model".to_owned(),
                    variant: "granite".to_owned(),
                },
                at: None,
            },
        )];

        fixture.ws.record_resolved_installs(&resolved);
        fixture.ws.record_resolved_installs(&resolved);

        let installed = fixture.ws.installed_models();
        assert_eq!(installed.len(), 1, "one record per variant, however many boots fetch it");
        assert_eq!(installed[0].variant, "granite");
        assert_eq!(installed[0].file, "granite-Q4_K_M.gguf");
        assert_eq!(installed[0].url, "https://weights.invalid/granite-Q4_K_M.gguf");
        assert_eq!(installed[0].facts.quant.as_deref(), Some("Q4_K_M"));
    }

    /// A runtime pick was installed before it could be picked, so
    /// recording never touches it: its record is what the pick resolves
    /// through in the first place.
    #[tokio::test]
    async fn a_runtime_pick_is_not_recorded_again() {
        let fixture = fixture_with(None, None);
        record_installed_model(&fixture.ws, "picked");
        let resolved = vec![(
            DictateRole::Transcribing,
            ActiveModel {
                role: DictateRole::Transcribing,
                spec: forge_dictate::ModelSpec::cohere_transcribe_q4_k_m(),
                from: ActiveFrom::Installed { variant: "picked".to_owned() },
                at: Some("2026-10-06T01:00:00Z".to_owned()),
            },
        )];

        fixture.ws.record_resolved_installs(&resolved);

        let installed = fixture.ws.installed_models();
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].file, "picked-Q4_K_M.gguf", "the record the pick came from");
    }

    /// The next *settled* re-read the install or activation pushes, on the
    /// catalogue check's own fifteen-second budget: a frame whose work has
    /// finished (`idle`) or stopped (`failed`). The progress frames in
    /// between are the page's, and a test that wants them collects its own.
    async fn await_models(
        updates: &mut tokio::sync::mpsc::UnboundedReceiver<SessionUpdate>,
    ) -> DictateModelsSnapshot {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                match updates.recv().await {
                    Some(SessionUpdate::DictateModelsChanged { models }) => {
                        let running = matches!(models.install, InstallState::Downloading { .. })
                            || matches!(models.activate, ActivateState::Activating { .. });
                        if !running {
                            break models;
                        }
                    }
                    Some(_) => {}
                    None => panic!("the subscription must stay attached"),
                }
            }
        })
        .await
        .expect("the install must land inside fifteen seconds")
    }

    /// A variant's doc names the file, the bytes arrive whole, and the
    /// model is recorded as installed: the record the next boot and the
    /// page's own reads both answer from.
    #[tokio::test]
    async fn an_install_lands_the_file_and_records_it() {
        let Fixture { ws, mut updates, .. } = fixture();
        ws.dictate_catalogue.lock().catalogue = Some(catalogue_of(vec![entry_under(
            "candidate",
            r#"["en"]"#,
            300.0,
            4.0,
            6,
            "mit",
            "MIT",
        )]));
        let file = "candidate-Q4_K_M.gguf";
        let base = serve_with(|base| {
            vec![
                ("/docs/candidate.md".to_owned(), 200, doc_body(base, "candidate", file)),
                (format!("/weights/{file}"), 200, b"111111".to_vec()),
            ]
        });
        *ws.test_catalogue_source.lock() = Some(source(&base));

        ws.dispatch(Command::DictateInstall { variant: "candidate".to_owned() })
            .expect("the install dispatches");

        let landed = await_models(&mut updates).await;
        assert!(
            matches!(landed.install, InstallState::Idle),
            "a clean install lands back at idle, got {:?}",
            landed.install
        );
        assert_eq!(landed.installed.len(), 1, "one file, one record");
        let record = &landed.installed[0];
        assert_eq!(record.variant, "candidate");
        assert_eq!(record.file, file);
        assert_eq!(record.url, format!("{base}/weights/{file}"), "the doc's own URL, kept");
        assert_eq!(record.size, 6);
        assert!(!record.at.is_empty(), "the record carries when it landed");
        assert_eq!(ws.installed_models().len(), 1, "and it reads back after the push");
    }

    /// Review Focus 2: bytes that disagree with the entry's own figure land
    /// failed, both numbers named, and nothing is recorded as installed.
    #[tokio::test]
    async fn an_install_whose_bytes_disagree_with_the_entry_lands_failed_and_records_nothing() {
        let Fixture { ws, mut updates, .. } = fixture();
        ws.dictate_catalogue.lock().catalogue = Some(catalogue_of(vec![entry_under(
            "candidate",
            r#"["en"]"#,
            300.0,
            4.0,
            6,
            "mit",
            "MIT",
        )]));
        let file = "candidate-Q4_K_M.gguf";
        let base = serve_with(|base| {
            vec![
                ("/docs/candidate.md".to_owned(), 200, doc_body(base, "candidate", file)),
                (format!("/weights/{file}"), 200, b"11111".to_vec()),
            ]
        });
        *ws.test_catalogue_source.lock() = Some(source(&base));

        ws.dispatch(Command::DictateInstall { variant: "candidate".to_owned() })
            .expect("the install dispatches");

        let landed = await_models(&mut updates).await;
        let InstallState::Failed { file: failed, reason } = &landed.install else {
            panic!("a size disagreement must land failed, got {:?}", landed.install);
        };
        assert_eq!(failed, file);
        assert!(
            reason.contains("5 bytes") && reason.contains("expected 6"),
            "the reason names both lengths, got: {reason}"
        );
        assert!(landed.installed.is_empty(), "nothing is recorded for a file that failed");
        assert!(ws.installed_models().is_empty());
    }

    /// A doc that carries no download for the quant the entry names - a
    /// table that moved, or one upstream trimmed - lands failed naming the
    /// file, rather than fetching some other row.
    #[tokio::test]
    async fn an_install_whose_doc_names_no_download_lands_failed() {
        let Fixture { ws, mut updates, .. } = fixture();
        ws.dictate_catalogue.lock().catalogue = Some(catalogue_of(vec![entry_under(
            "candidate",
            r#"["en"]"#,
            300.0,
            4.0,
            6,
            "mit",
            "MIT",
        )]));
        let file = "candidate-Q4_K_M.gguf";
        let base = serve_with(|_| {
            vec![(String::from("/docs/candidate.md"), 200, b"# candidate\n\nprose only\n".to_vec())]
        });
        *ws.test_catalogue_source.lock() = Some(source(&base));

        ws.dispatch(Command::DictateInstall { variant: "candidate".to_owned() })
            .expect("the install dispatches");

        let landed = await_models(&mut updates).await;
        let InstallState::Failed { file: failed, reason } = &landed.install else {
            panic!("a doc without the file must land failed, got {:?}", landed.install);
        };
        assert_eq!(failed, file, "the file the entry names, so the page says which one stopped");
        assert!(
            reason.contains("no download"),
            "the reason names the step that stopped, got: {reason}"
        );
        assert!(ws.installed_models().is_empty());
    }

    /// **The variant whose only document is the README.** Moonshine's
    /// language fine-tunes - and breeze - have no page in the docs tree at
    /// all, so the download link can only come from the published repo's
    /// README: a 404 on the doc must fall through to it, not end the
    /// install. (Measured against the real feed: 60 of 74 variants have a
    /// per-variant doc, 14 have only the README, and every one is reachable
    /// from one of the two.)
    #[tokio::test]
    async fn an_install_reads_the_link_from_the_readme_when_the_doc_is_missing() {
        let Fixture { ws, mut updates, .. } = fixture();
        ws.dictate_catalogue.lock().catalogue = Some(catalogue_of(vec![
            forge_dictate::catalogue::parse_entry(
                r#"{"schema":"transcribe-catalog-v1","variant":"moonshine-base-ar",
                    "published_repo":"handy-computer/moonshine-base-ar-gguf",
                    "downloads":[{"quant":"Q8_0","filename":"moonshine-base-ar-Q8_0.gguf","size_bytes":6}]}"#,
            )
            .expect("the synthetic entry parses"),
        ]));
        let file = "moonshine-base-ar-Q8_0.gguf";
        let base = serve_with(|base| {
            vec![
                // Nothing under /docs for this variant: unrouted paths 404,
                // which is exactly what the real tree answers for it.
                (
                    String::from("/repos/handy-computer/moonshine-base-ar-gguf/raw/main/README.md"),
                    200,
                    format!(
                        "# moonshine-base-ar\n\n| Quant | Download |\n| --- | --- |\n\
                         | Q8_0 | [{file}]({base}/weights/{file}) |\n"
                    )
                    .into_bytes(),
                ),
                (format!("/weights/{file}"), 200, b"111111".to_vec()),
            ]
        });
        *ws.test_catalogue_source.lock() = Some(source(&base));

        ws.dispatch(Command::DictateInstall { variant: "moonshine-base-ar".to_owned() })
            .expect("the install dispatches");

        let landed = await_models(&mut updates).await;
        assert!(matches!(landed.install, InstallState::Idle), "got {:?}", landed.install);
        assert_eq!(landed.installed.len(), 1);
        assert_eq!(landed.installed[0].file, file);
        assert_eq!(landed.installed[0].url, format!("{base}/weights/{file}"));
    }
    /// **A Hub entry installs by its own url and verifies its own digest.**
    /// The speech feed's docs publish neither, so every other test here
    /// exercises the README-derived path; this one is the Hub's - the url
    /// the entry carries is what is fetched, and a digest that disagrees
    /// with the bytes fails the install by name rather than landing a file.
    #[tokio::test]
    async fn a_hub_entry_installs_by_its_url_and_verifies_its_digest() {
        let body = b"hubbytes".to_vec();
        let good = "0c6bee82a304781cb9fefa1dda4bfa5f5b8be07d9f6f77918f2d96854ce5165e";
        let base = serve(vec![("/weights/norm-a-Q4_K_M.gguf", 200, body)]);
        let entry = |digest: &str| {
            forge_dictate::catalogue::parse_entry(&format!(
                r#"{{"schema":"transcribe-catalog-v1","variant":"owner/norm-a",
                    "kind":"normalizer",
                    "downloads":[{{"quant":"Q4_K_M","filename":"norm-a-Q4_K_M.gguf",
                        "size_bytes":8,"sha256":"{digest}",
                        "url":"{base}/weights/norm-a-Q4_K_M.gguf"}}]}}"#
            ))
            .expect("the synthetic entry parses")
        };

        // The url the entry carries is the one the download resolves to.
        let held = entry(good);
        assert_eq!(
            held.downloads[0].url.as_deref(),
            Some(format!("{base}/weights/norm-a-Q4_K_M.gguf").as_str()),
            "the entry's own url rides its download"
        );

        // The digest the bytes carry: the install lands.
        let Fixture { ws, mut updates, .. } = fixture();
        ws.dictate_catalogue.lock().catalogue = Some(catalogue_of(vec![held]));
        ws.dispatch(Command::DictateInstall { variant: "owner/norm-a".to_owned() })
            .expect("the install dispatches");
        let landed = await_models(&mut updates).await;
        assert!(matches!(landed.install, InstallState::Idle), "got {:?}", landed.install);
        assert_eq!(landed.installed.len(), 1, "a matching digest lands the file");

        // A digest the bytes do not match: the install fails by name.
        let Fixture { ws, mut updates, .. } = fixture();
        ws.dictate_catalogue.lock().catalogue = Some(catalogue_of(vec![entry(
            "0000000000000000000000000000000000000000000000000000000000000000",
        )]));
        ws.dispatch(Command::DictateInstall { variant: "owner/norm-a".to_owned() })
            .expect("the install dispatches");
        let landed = await_models(&mut updates).await;
        let InstallState::Failed { reason, .. } = &landed.install else {
            panic!("a wrong digest must fail the install, got {:?}", landed.install)
        };
        assert!(
            reason.contains("hashes to") && reason.contains("expected"),
            "the refusal names what disagreed, got: {reason}"
        );
        assert!(landed.installed.is_empty(), "and nothing was recorded");
    }

    /// Review Focus 5: with `[dictate]` off there is no models directory to
    /// install into and no engine to run one, so the command refuses by name.
    #[test]
    fn an_install_refuses_by_name_with_dictation_off() {
        let (ws, _updates) = Workspace::testing_stub();

        let err = ws
            .dispatch(Command::DictateInstall { variant: "candidate".to_owned() })
            .expect_err("no dictation, no install");

        assert!(matches!(err, DispatchError::DictateOff), "got: {err:?}");
    }

    /// A second install while one is downloading is refused, and the one in
    /// flight is untouched: its progress bar is the answer.
    #[test]
    fn an_install_while_one_is_downloading_is_refused() {
        let (ws, _updates, _models) = enabled_stub();
        let in_flight =
            InstallState::Downloading { file: "half.gguf".to_owned(), got: 1, total: 6 };
        *ws.dictate_install.lock() = in_flight.clone();

        let err = ws
            .dispatch(Command::DictateInstall { variant: "candidate".to_owned() })
            .expect_err("a second install must be refused");

        assert!(matches!(err, DispatchError::Installing), "got: {err:?}");
        assert_eq!(ws.dictate_install(), in_flight, "the refusal leaves the install in flight");
    }

    /// Installing the same file again replaces its record rather than
    /// stacking a second one: the set is one entry per file, which is what
    /// the page and the active-model resolution both read.
    #[tokio::test]
    async fn reinstalling_the_same_file_replaces_its_record() {
        let Fixture { ws, mut updates, .. } = fixture();
        ws.dictate_catalogue.lock().catalogue = Some(catalogue_of(vec![entry_under(
            "candidate",
            r#"["en"]"#,
            300.0,
            4.0,
            6,
            "mit",
            "MIT",
        )]));
        let file = "candidate-Q4_K_M.gguf";
        let base = serve_with(|base| {
            vec![
                ("/docs/candidate.md".to_owned(), 200, doc_body(base, "candidate", file)),
                (format!("/weights/{file}"), 200, b"111111".to_vec()),
            ]
        });
        *ws.test_catalogue_source.lock() = Some(source(&base));

        let install = || Command::DictateInstall { variant: "candidate".to_owned() };

        ws.dispatch(install()).expect("the first install dispatches");
        let first = await_models(&mut updates).await;
        assert_eq!(first.installed.len(), 1);

        ws.dispatch(install()).expect("the second install dispatches");
        let second = await_models(&mut updates).await;

        assert_eq!(second.installed.len(), 1, "the record is replaced, not duplicated");
        assert_eq!(second.installed[0].file, file);
    }

    /// **The page's progress line moves because the server pushes on it.**
    /// A state the core records but never sends is a page that sits at
    /// `0 of 0` until the install is over - measured live on the stack, and
    /// the reason this test exists.
    #[tokio::test]
    async fn an_install_pushes_its_progress_while_it_downloads() {
        let mut fixture = fixture();
        fixture.ws.dictate_catalogue.lock().catalogue = Some(catalogue_of(vec![entry_under(
            "candidate",
            r#"["en"]"#,
            300.0,
            4.0,
            6,
            "mit",
            "MIT",
        )]));
        let file = "candidate-Q4_K_M.gguf";
        let base = serve_with(|base| {
            vec![
                ("/docs/candidate.md".to_owned(), 200, doc_body(base, "candidate", file)),
                (format!("/weights/{file}"), 200, b"111111".to_vec()),
            ]
        });
        *fixture.ws.test_catalogue_source.lock() = Some(source(&base));

        fixture
            .ws
            .dispatch(Command::DictateInstall { variant: "candidate".to_owned() })
            .expect("the install dispatches");

        let mut downloading = false;
        let mut named = 0;
        let mut moved = false;
        let mut finished = false;
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            while let Some(update) = fixture.updates.recv().await {
                let SessionUpdate::DictateModelsChanged { models } = update else { continue };
                match &models.install {
                    InstallState::Downloading { file: seen, got, .. } => {
                        downloading = true;
                        // **Bytes on the transfer, not just the state.** The
                        // pre-spawn frame already says `downloading` with the
                        // file named, so a test that stopped there would pass
                        // with the progress frames never sent at all.
                        if *got > 0 {
                            moved = true;
                        }
                        if seen == file {
                            named += 1;
                        }
                    }
                    InstallState::Idle => {
                        finished = true;
                        break;
                    }
                    InstallState::Failed { .. } => break,
                }
            }
        })
        .await
        .expect("the install must land inside fifteen seconds");

        assert!(downloading, "no frame carried the download while it ran");
        assert!(named > 0, "the frames must name the file they are about");
        assert!(moved, "no frame carried bytes moving");
        assert!(finished, "the install must end in a frame");
    }

    /// A file of bytes that are not a model, where the engine would look
    /// for one.
    fn write_unloadable_model(ws: &Workspace, file: &str) {
        let path = ws.config.dictate.models_dir().expect("the fixture sets one").join(file);
        std::fs::write(&path, b"these bytes are not a model")
            .unwrap_or_else(|error| panic!("write {path:?}: {error}"));
    }

    /// Review Focus 3: an activation whose model will not load lands
    /// failed, and the engine slot and the roles are left as they were.
    #[tokio::test]
    async fn an_activation_that_cannot_load_leaves_the_previous_engine_in_place() {
        // The cleanup role off, so the engine build names exactly one
        // model - the one under test - and the failure cannot come from
        // another model's file being missing. The whole fixture is bound:
        // destructuring the tempdirs away would delete the models dir
        // under the test.
        let mut fixture = fixture_built(|config| config.dictate.normalizer = false);
        record_installed_model(&fixture.ws, "broken");
        write_unloadable_model(&fixture.ws, "broken-Q4_K_M.gguf");

        fixture
            .ws
            .dispatch(Command::DictateActivate {
                role: DictateRole::Transcribing,
                file: "broken-Q4_K_M.gguf".to_owned(),
            })
            .expect("the activation dispatches");

        let landed = await_models(&mut fixture.updates).await;
        let ActivateState::Failed { role, file, reason } = &landed.activate else {
            panic!("an unloadable model must land failed, got {:?}", landed.activate);
        };
        assert_eq!(*role, DictateRole::Transcribing);
        assert_eq!(file, "broken-Q4_K_M.gguf");
        assert!(
            reason.contains("broken-Q4_K_M.gguf"),
            "the failure must be about the model that was activated, got: {reason}"
        );
        assert!(fixture.ws.dictate.engine.lock().is_none(), "nothing was swapped in");
        assert_eq!(
            role_model(&fixture.ws.active_models(), DictateRole::Transcribing).from,
            ActiveFrom::Pin,
            "and the role still answers what it did before"
        );
    }

    /// Review Focus 4: a live take refuses the swap by name - the seat it
    /// is on - and nothing starts.
    #[tokio::test]
    async fn an_activation_refuses_while_a_take_is_live() {
        let fixture = fixture_with(None, None);
        record_installed_model(&fixture.ws, "candidate");
        let (stop, _stop_rx) = tokio::sync::mpsc::channel(1);
        fixture.ws.dictate_runtime.lock().recordings.insert(
            crate::SessionSlot::new("Busytools", "forge", "worker"),
            crate::dictate::LiveRecording { stop, sink: None, initiator: None },
        );

        let err = fixture
            .ws
            .dispatch(Command::DictateActivate {
                role: DictateRole::Transcribing,
                file: "candidate-Q4_K_M.gguf".to_owned(),
            })
            .expect_err("a live take refuses the swap");

        assert!(
            matches!(&err, DispatchError::TakeLive { holder } if holder == "Busytools/forge/worker"),
            "the refusal names the seat whose take is live, got: {err:?}"
        );
        assert_eq!(fixture.ws.dictate_activate(), ActivateState::Idle, "and nothing started");
    }

    /// A failed activation changes nothing a restart would answer: the
    /// runtime pick still names the model the machine was running.
    #[tokio::test]
    async fn a_failed_activation_leaves_the_record_and_the_running_model_alone() {
        let mut fixture = fixture_built(|config| config.dictate.normalizer = false);
        record_installed_model(&fixture.ws, "broken");
        record_installed_model(&fixture.ws, "picked");
        record_active(&fixture.ws, DictateRole::Transcribing, "picked");
        write_unloadable_model(&fixture.ws, "broken-Q4_K_M.gguf");

        fixture
            .ws
            .dispatch(Command::DictateActivate {
                role: DictateRole::Transcribing,
                file: "broken-Q4_K_M.gguf".to_owned(),
            })
            .expect("the activation dispatches");
        let landed = await_models(&mut fixture.updates).await;
        assert!(
            matches!(landed.activate, ActivateState::Failed { .. }),
            "got {:?}",
            landed.activate
        );

        let resolved =
            fixture.ws.resolve_active(&settings_of(&fixture.ws)).await.expect("the pick resolves");
        assert_eq!(
            role_model(&resolved, DictateRole::Transcribing).spec.file,
            "picked-Q4_K_M.gguf",
            "a restart still answers the model the machine was running"
        );
    }

    /// The config pin refuses activation by name; downloads and benchmarks
    /// stay allowed, but the runtime cannot move the role.
    #[tokio::test]
    async fn an_activation_on_a_pinned_role_is_refused_by_name() {
        let fixture = fixture_with(Some("keyed"), None);
        record_installed_model(&fixture.ws, "keyed");

        let err = fixture
            .ws
            .dispatch(Command::DictateActivate {
                role: DictateRole::Transcribing,
                file: "keyed-Q4_K_M.gguf".to_owned(),
            })
            .expect_err("forge.toml pins the role");

        assert!(
            matches!(&err, DispatchError::PinnedRole { key } if key == "transcribe_model"),
            "the refusal names the [dictate] key, got: {err:?}"
        );
    }

    /// A deactivation that cannot load the default keeps the pick: the
    /// record is dropped only after the swap lands, so the page's rows,
    /// the next boot and the running engine never disagree.
    #[tokio::test]
    async fn a_deactivation_whose_swap_fails_keeps_the_pick() {
        let mut fixture = fixture_built(|config| config.dictate.normalizer = false);
        record_installed_model(&fixture.ws, "picked");
        record_active(&fixture.ws, DictateRole::Transcribing, "picked");
        // The role runs the pick, and the compiled default is not on disk.
        fixture.ws.dictate.set_active(&[(
            DictateRole::Transcribing,
            ActiveModel {
                role: DictateRole::Transcribing,
                spec: ModelSpec {
                    file: "picked-Q4_K_M.gguf".to_owned(),
                    url: "https://weights.invalid/picked-Q4_K_M.gguf".to_owned(),
                    size: 6,
                    sha256: None,
                    facts: ModelFacts { quant: Some("Q4_K_M".to_owned()), ..ModelFacts::default() },
                },
                from: ActiveFrom::Installed { variant: "picked".to_owned() },
                at: Some("2026-10-06T01:00:00Z".to_owned()),
            },
        )]);

        fixture
            .ws
            .dispatch(Command::DictateDeactivate { role: DictateRole::Transcribing })
            .expect("the deactivation dispatches");

        let landed = await_models(&mut fixture.updates).await;
        assert!(
            matches!(landed.activate, ActivateState::Failed { .. }),
            "got {:?}",
            landed.activate
        );
        let db = fixture.ws.db.lock();
        let choice = crate::store::dictate_models::active(
            db.as_ref().expect("the fixture installs a store"),
            DictateRole::Transcribing,
        )
        .expect("the store reads");
        assert_eq!(
            choice.map(|choice| choice.variant),
            Some("picked".to_owned()),
            "the pick is dropped only once the swap lands"
        );
    }
}
