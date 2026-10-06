//! Installing a model from the feed, and resolving which model each role
//! runs: the doc's own download link, a size-checked fetch into the models
//! directory, the record of what arrived, and the config-key / runtime-pick
//! / compiled-pin order an active model comes from.
//!
//! **A download is checked by the entry's own byte length, and by whether
//! the engine can load it - never by a digest, because upstream publishes
//! none for these weights.** Nothing here, in its copy or in its errors,
//! may call a downloaded file verified.

use std::cell::Cell;
use std::sync::Arc;

use forge_dictate::catalogue::{
    CatalogueEntry, CatalogueSource, Download, doc_links, fetch_doc,
};
use forge_dictate::{ModelFacts, ModelSpec, Progress};
use serde::{Deserialize, Serialize};

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
    Downloading { file: String, got: u64, total: u64 },
    Failed { file: String, reason: String },
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
        db.as_ref()
            .map(|db| crate::store::dictate_models::installed(db).unwrap_or_default())
            .unwrap_or_default()
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

    fn record_installed(&self, model: InstalledModel) {
        let db = self.db.lock();
        if let Some(db) = db.as_ref()
            && let Err(error) = crate::store::dictate_models::record_installed(db, &model)
        {
            tracing::warn!(
                event_name = "dictate_install_record_failed",
                %error,
                file = %model.file,
                "the downloaded model was not recorded; the next boot fetches it again"
            );
        }
    }

    /// The page's re-read, pushed the way a landed check pushes one.
    fn push_models(&self) {
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
            return self.fail_install(
                download.filename,
                "no models directory is configured".to_owned(),
            );
        };
        let mut cfg = self.config.dictate.to_config();
        cfg.models_dir = Some(dir);
        cfg.asr_model = spec.clone();
        cfg.normalizer = None;

        self.set_install_file(&download.filename, download.size_bytes);
        let this = Arc::clone(&self);
        let file = download.filename.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            // The push rides the whole-percent change: a tick per progress
            // event would re-encode the page's snapshot thousands of times
            // over a 279 MB file, and a percent is what a reader sees move.
            let last = Cell::new(u64::MAX);
            forge_dictate::prepare(&cfg, |progress| {
                if let Progress::Downloading { downloaded, total, .. } = progress {
                    let percent = if total == 0 { 0 } else { downloaded * 100 / total };
                    if percent != last.get() {
                        last.set(percent);
                        this.note_download(&file, downloaded, total);
                    }
                }
                std::ops::ControlFlow::Continue(())
            })
        })
        .await;

        match prepared {
            Ok(Ok(())) => {
                self.record_installed(InstalledModel {
                    variant,
                    file: spec.file,
                    url: spec.url,
                    size: spec.size,
                    facts: spec.facts,
                    at: rfc3339_now(),
                });
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

/// The spec one feed entry's doc describes for the quant a row draws: the
/// doc's own URL for the file, the entry's own byte length, and the facts
/// both carry.
async fn spec_for_entry(
    source: CatalogueSource,
    entry: &CatalogueEntry,
    download: &Download,
) -> Result<ModelSpec, String> {
    let variant = entry.variant.clone();
    let doc = tokio::task::spawn_blocking(move || fetch_doc(&source, &variant))
        .await
        .map_err(|join| join.to_string())?
        .map_err(|error| error.to_string())?;
    let Some((_, url)) = doc_links(&doc).into_iter().find(|(file, _)| file == &download.filename)
    else {
        return Err(format!("the doc carries no download for {}", download.filename));
    };
    Ok(forge_dictate::spec_for_download(
        &download.filename,
        &url,
        download.size_bytes,
        ModelFacts {
            quant: Some(download.quant.clone()),
            params: Some(entry.params),
            license: entry.license.as_ref().map(|license| license.display.clone()),
            runtime: Some("transcribe.cpp".to_owned()),
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
                let Some(installed) = installed else {
                    return Err(format!(
                        "the runtime pick for {} names {}, which no installed model carries",
                        crate::store::dictate_models::role_key(role),
                        choice.variant
                    ));
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
    fn active_choice(&self, role: DictateRole) -> Option<ActiveChoice> {
        let db = self.db.lock();
        db.as_ref().and_then(|db| crate::store::dictate_models::active(db, role).ok().flatten())
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
            self.record_installed(InstalledModel {
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
        let at = rfc3339_now();
        let next: Vec<(DictateRole, ActiveModel)> = current
            .into_iter()
            .map(|(r, model)| {
                if r == role {
                    (role, ActiveModel {
                        role,
                        spec: spec.clone(),
                        from: from.clone(),
                        at: Some(at.clone()),
                    })
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
        self.dictate.mark_all(crate::dictate::DictateModelState::Ready);
        Ok(at)
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
fn rfc3339_now() -> String {
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
        ws.record_installed(InstalledModel {
            variant: variant.to_owned(),
            file: format!("{variant}-Q4_K_M.gguf"),
            url: format!("https://weights.invalid/{variant}-Q4_K_M.gguf"),
            size: 6,
            facts: ModelFacts { quant: Some("Q4_K_M".to_owned()), ..ModelFacts::default() },
            at: "2026-10-06T00:00:00Z".to_owned(),
        });
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
    fn role_model<'r>(
        resolved: &'r [(DictateRole, ActiveModel)],
        role: DictateRole,
    ) -> &'r ActiveModel {
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
            ActiveFrom::Config {
                key: "transcribe_model".to_owned(),
                variant: "keyed".to_owned(),
            },
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
        assert_eq!(
            transcribing.from,
            ActiveFrom::Installed { variant: "picked".to_owned() }
        );
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
        assert_eq!(
            transcribing.from,
            ActiveFrom::Installed { variant: "picked".to_owned() }
        );
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

        let err = fixture
            .ws
            .resolve_active(&settings)
            .await
            .expect_err("nothing names the variant");

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

    /// The next re-read the install pushes, on the catalogue check's own
    /// fifteen-second budget.
    async fn await_models(
        updates: &mut tokio::sync::mpsc::UnboundedReceiver<SessionUpdate>,
    ) -> DictateModelsSnapshot {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                match updates.recv().await {
                    Some(SessionUpdate::DictateModelsChanged { models }) => break models,
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
        ws.dictate_catalogue.lock().catalogue =
            Some(catalogue_of(vec![entry_under("candidate", r#"["en"]"#, 300.0, 4.0, 6, "mit", "MIT")]));
        let file = "candidate-Q4_K_M.gguf";
        let base = serve_with(|base| {
            vec![
                (format!("/docs/candidate.md"), 200, doc_body(base, "candidate", file)),
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
        ws.dictate_catalogue.lock().catalogue =
            Some(catalogue_of(vec![entry_under("candidate", r#"["en"]"#, 300.0, 4.0, 6, "mit", "MIT")]));
        let file = "candidate-Q4_K_M.gguf";
        let base = serve_with(|base| {
            vec![
                (format!("/docs/candidate.md"), 200, doc_body(base, "candidate", file)),
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
        ws.dictate_catalogue.lock().catalogue =
            Some(catalogue_of(vec![entry_under("candidate", r#"["en"]"#, 300.0, 4.0, 6, "mit", "MIT")]));
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
            reason.contains("no download for"),
            "the reason names the step that stopped, got: {reason}"
        );
        assert!(ws.installed_models().is_empty());
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
        ws.dictate_catalogue.lock().catalogue =
            Some(catalogue_of(vec![entry_under("candidate", r#"["en"]"#, 300.0, 4.0, 6, "mit", "MIT")]));
        let file = "candidate-Q4_K_M.gguf";
        let base = serve_with(|base| {
            vec![
                (format!("/docs/candidate.md"), 200, doc_body(base, "candidate", file)),
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
