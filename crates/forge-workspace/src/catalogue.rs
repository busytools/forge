//! The dictation model catalogue: the feed's check and the models page's
//! view.
//!
//! The feed itself is `forge-dictate`'s; this module folds it into what the
//! page draws and owns the rule that decides when a catalogue entry counts
//! as an update to the model in use.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use forge_dictate::catalogue::{Catalogue, CatalogueEntry};
use forge_dictate::{ModelFacts, ModelSpec};

use crate::dictate::{DictateModelState, DictateRole, DictateSnapshot};

/// A fetched feed older than this refreshes at boot. Upstream's cadence is
/// weeks, so a day is what keeps a restart from being the only way a new
/// variant is ever seen, while a boot nobody asked for a download stays
/// off the network.
pub(crate) const REFRESH_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// The quants a row's download size prefers, most preferred first: the Q4
/// build is what a dictation machine runs, and the chain after it keeps a
/// row that ships no Q4 sized rather than sizeless.
pub(crate) use forge_dictate::catalogue::PREFERRED_DOWNLOADS;

/// Everything the models page draws, in one read.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DictateModelsSnapshot {
    /// Whether `[dictate] enabled` is set. Carried rather than inferred
    /// from an empty `in_use`: a switched-off section and a failed
    /// preflight both empty it.
    pub enabled: bool,
    /// Where the dictation models land.
    pub models_dir: Option<PathBuf>,
    /// The models forge is configured to run, pins and live state.
    pub in_use: Vec<InUseModel>,
    /// The catalogue's own freshness.
    pub check: CatalogueCheck,
    /// Per in-service model, the catalogue entry worth adopting, when one
    /// is.
    pub updates: Vec<ModelUpdate>,
    /// The whole feed, for the page's search list.
    pub rows: Vec<CatalogueRow>,
    /// Where the last model install got to.
    pub install: crate::install::InstallState,
    /// Where the last model activation got to.
    pub activate: crate::install::ActivateState,
    /// Every model downloaded from the feed on this machine, oldest first.
    pub installed: Vec<crate::install::InstalledModel>,
    /// Where the last bench got to: idle, running with its tick, or
    /// failed with its reason.
    pub bench: crate::bench::BenchState,
    /// What benches have measured on this machine, newest first.
    pub results: Vec<crate::bench::BenchResult>,
    /// The read-aloud set: whether this machine has one, and its passage.
    pub read_aloud: crate::bench::ReadAloudState,
}

/// What the last catalogue check did.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CatalogueCheck {
    /// Nothing has fetched on this machine.
    Never,
    /// A fetch is in flight.
    Checking,
    /// The last fetch answered. `at` is when it ran, RFC 3339.
    Fresh {
        at: String,
        /// The upstream release the feed stood at, when it answered.
        release: Option<String>,
        /// How many listed documents did not become entries.
        skipped: usize,
    },
    /// The last fetch failed. The rows, when there are any, are the
    /// last-known feed; nothing in use changed.
    Unreachable { error: String },
}

/// One model forge runs, as the page's IN USE rows draw it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InUseModel {
    pub role: DictateRole,
    pub file: String,
    pub size: u64,
    /// The digest the spec declares, or `None` for an installed model whose
    /// upstream publishes none: nothing may print a digest for one.
    pub sha256: Option<String>,
    /// The preflight snapshot's own state: pending through ready.
    pub state: DictateModelState,
    /// The facts the spec declares.
    pub facts: ModelFacts,
    /// The feed's entry for this file, when it carries one.
    pub catalogue: Option<CatalogueJoin>,
    /// Where this role's model came from: the config key, the last runtime
    /// pick, or the compiled pin.
    pub from: crate::install::ActiveFrom,
    /// RFC 3339, when a runtime pick chose it.
    pub at: Option<String>,
}

/// What the feed says about a file the pin names.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CatalogueJoin {
    pub variant: String,
    pub display_name: String,
    /// The feed's own byte length for the file. Differing from the pin's
    /// is the feed saying the file was rebuilt.
    pub size_bytes: u64,
    pub streaming: bool,
    pub languages: Vec<String>,
    pub speed: Option<SpeedFact>,
}

/// One measured speed row, as the page's speed line draws it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SpeedFact {
    pub machine: String,
    pub backend: String,
    pub quant: String,
    pub xrt_wall: f64,
}

/// One catalogue entry, as the page's candidate rows draw it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CatalogueRow {
    pub variant: String,
    pub display_name: String,
    pub family: String,
    /// The parameter count the source published, or `None` when it published
    /// none - a row drawing `0M params` would claim a measurement.
    pub params: Option<u64>,
    pub license: Option<String>,
    pub languages: Vec<String>,
    pub streaming: bool,
    /// The preferred quant's size, when the entry ships one of them.
    pub download: Option<DownloadFact>,
    pub speed: Option<SpeedFact>,
    /// The comparison axis: FLEURS English where the entry carries it,
    /// else its own headline benchmark.
    pub wer: Option<WerFact>,
    /// What the entry is for: which role's candidates it belongs in.
    pub kind: forge_dictate::catalogue::EntryKind,
    /// Where a reader can read about it, when the feed names its own page.
    /// The Hub's entries do; the speech feed's leave this `None` and the
    /// page opens the entry's document instead.
    pub url: Option<String>,
    /// How many times the Hub has served it - the only pre-run signal a
    /// cleanup candidate carries, and what orders a sweep. The speech feed
    /// carries no count.
    pub download_count: Option<u64>,
}

/// One downloadable quantisation's size.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DownloadFact {
    pub quant: String,
    pub size_bytes: u64,
}

/// One accuracy figure and the benchmark it was measured on.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WerFact {
    pub dataset: String,
    pub split: String,
    pub language: String,
    pub err_pct: f64,
}

/// The comparison one role's model is read against: the model in use, and
/// every English entry it can be compared with, ranked the way the rule
/// ranks them.
///
/// **The whole comparison crosses, not just the winner.** The rule that
/// picks a candidate is the thing a reader wants to check, and a page that
/// showed only the pick would be asking to be trusted.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelUpdate {
    pub role: DictateRole,
    /// The in-service file this is about.
    pub file: String,
    /// The numbers the in-service model's own catalogue entry carries,
    /// which is the baseline every candidate row is read against.
    pub current: CurrentFacts,
    /// The comparable entries other than the one in use, fastest first
    /// with the sharper breaking a tie - the order the rule walks.
    pub candidates: Vec<CandidateRow>,
}

/// One compared entry, and what the rule makes of it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CandidateRow {
    pub row: CatalogueRow,
    pub verdict: UpdateVerdict,
}

/// Why a candidate is, or is not, the one the page proposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateVerdict {
    /// The fastest entry that beats the model in use on both axes: what the
    /// page proposes adopting.
    Recommended,
    /// It beats the model in use on both axes, and something faster does
    /// too.
    BeatsBoth,
    /// It is no faster than the model in use, whatever its error rate.
    Slower,
    /// It is faster and no more accurate.
    Blunter,
}

/// The in-service side of an update's comparison.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CurrentFacts {
    pub speed_x: f64,
    pub fleurs_en_wer: f64,
}

/// Fold one entry into the row a candidate list draws.
pub(crate) fn row_for(entry: &CatalogueEntry) -> CatalogueRow {
    CatalogueRow {
        variant: entry.variant.clone(),
        display_name: entry.display_name.clone(),
        family: entry.family.clone(),
        params: entry.params,
        license: entry.license.as_ref().map(|license| license.display.clone()),
        languages: entry.languages.clone(),
        streaming: entry.streaming(),
        download: download_fact(entry),
        speed: speed_fact(entry),
        wer: wer_fact(entry),
        kind: entry.kind,
        // Only the Hub's entries name a page of their own; a speech entry's
        // row keeps the document the page already opens for it.
        url: match entry.kind {
            forge_dictate::catalogue::EntryKind::Normalizer => {
                entry.published_repo.as_ref().map(|repo| format!("https://huggingface.co/{repo}"))
            }
            forge_dictate::catalogue::EntryKind::Asr => None,
        },
        download_count: entry.download_count,
    }
}

/// The speed row the entry's best wall-clock number came from.
fn speed_fact(entry: &CatalogueEntry) -> Option<SpeedFact> {
    let row = entry.m4_metal_speed_row()?;
    Some(SpeedFact {
        machine: row.machine.clone(),
        backend: row.backend.clone(),
        quant: row.quant.clone(),
        xrt_wall: row.xrt_wall?,
    })
}

/// The entry's preferred download, by the quantization chain the list
/// draws.
fn download_fact(entry: &CatalogueEntry) -> Option<DownloadFact> {
    PREFERRED_DOWNLOADS.iter().find_map(|quant| {
        entry.downloads.iter().find(|download| &download.quant == quant).map(|download| {
            DownloadFact { quant: download.quant.clone(), size_bytes: download.size_bytes }
        })
    })
}

/// The entry's comparison axis: FLEURS English where it exists, else the
/// entry's own headline row.
fn wer_fact(entry: &CatalogueEntry) -> Option<WerFact> {
    if let Some(err_pct) = entry.fleurs_en_wer() {
        return Some(WerFact {
            dataset: "fleurs".to_owned(),
            split: "test".to_owned(),
            language: "en".to_owned(),
            err_pct,
        });
    }
    let headline = entry.headline_benchmark.as_ref()?;
    Some(WerFact {
        dataset: headline.dataset.clone(),
        split: headline.split.clone(),
        language: headline.language.clone(),
        err_pct: entry.headline_wer()?,
    })
}

/// The feed's entry for a pinned file, joined by file name - the one
/// thing the pin and the feed share.
pub(crate) fn join_for(entries: &[CatalogueEntry], spec: &ModelSpec) -> Option<CatalogueJoin> {
    let entry = entry_for(entries, spec)?;
    let download = entry.download_for(&spec.file)?;
    Some(CatalogueJoin {
        variant: entry.variant.clone(),
        display_name: entry.display_name.clone(),
        size_bytes: download.size_bytes,
        streaming: entry.streaming(),
        languages: entry.languages.clone(),
        speed: speed_fact(entry),
    })
}

fn entry_for<'e>(entries: &'e [CatalogueEntry], spec: &ModelSpec) -> Option<&'e CatalogueEntry> {
    entries.iter().find(|entry| entry.download_for(&spec.file).is_some())
}

/// The IN USE rows: the model's declaration, the preflight state, the
/// feed's entry when there is one, and where the choice came from.
pub(crate) fn in_use(
    entries: &[CatalogueEntry],
    active: &[(DictateRole, crate::install::ActiveModel)],
    states: &DictateSnapshot,
) -> Vec<InUseModel> {
    active
        .iter()
        .map(|(role, model)| {
            let spec = &model.spec;
            let state = states
                .models
                .iter()
                .find(|row| row.file == spec.file)
                .map_or(DictateModelState::Pending, |row| row.state.clone());
            InUseModel {
                role: *role,
                file: spec.file.clone(),
                size: spec.size,
                sha256: spec.sha256.clone(),
                state,
                facts: spec.facts.clone(),
                catalogue: join_for(entries, spec),
                from: model.from.clone(),
                at: model.at.clone(),
            }
        })
        .collect()
}

/// The comparison one role's model is read against: the model in use and
/// every comparable entry, ranked the way the rule ranks them.
///
/// **The rule, in full, because the page tables it.** A candidate is
/// comparable when it carries English and the feed measured it on the same
/// two axes the model in use carries - m4-max Metal wall-clock realtime
/// factor, and FLEURS English word error rate. Comparable entries rank
/// fastest first, with the sharper breaking a tie. The first one that
/// beats the model in use on BOTH axes is the pick; the ones behind it
/// that also beat both are listed as such; the rest carry the one axis
/// they lose on.
///
/// **A non-commercial licence is a fact on the row, not a filter.**
/// Recommending a model used to mean pinning it into forge's shipped
/// defaults, where `-nc` bars it; what the page does now is download and
/// run one on this machine, which is the licence's own personal use. The
/// row states the licence so the constraint is read where the decision is.
pub(crate) fn updates_for(
    entries: &[CatalogueEntry],
    specs: &[(DictateRole, ModelSpec)],
) -> Vec<ModelUpdate> {
    specs
        .iter()
        .filter_map(|(role, spec)| {
            let joined = entry_for(entries, spec)?;
            // The pin's own byte length witnesses that the entry is
            // about the file in use: a rebuild under the same name
            // carries other bytes, and its measured rows are not this
            // file's.
            if joined.download_for(&spec.file)?.size_bytes != spec.size {
                return None;
            }
            let current_speed = joined.m4_metal_xrt_wall()?;
            let current_wer = joined.fleurs_en_wer()?;

            let mut ranked: Vec<(&CatalogueEntry, f64, f64)> = Vec::new();
            for entry in entries {
                if entry.variant == joined.variant {
                    continue;
                }
                if !entry.languages.iter().any(|language| language == "en") {
                    continue;
                }
                let (Some(speed), Some(wer)) = (entry.m4_metal_xrt_wall(), entry.fleurs_en_wer())
                else {
                    continue;
                };
                ranked.push((entry, speed, wer));
            }
            ranked.sort_by(|(_, a_speed, a_wer), (_, b_speed, b_wer)| {
                b_speed.total_cmp(a_speed).then(a_wer.total_cmp(b_wer))
            });

            let mut recommended = false;
            let candidates: Vec<CandidateRow> = ranked
                .into_iter()
                .map(|(entry, speed, wer)| {
                    let beats = speed > current_speed && wer < current_wer;
                    // The first beater in the ranked order is the fastest
                    // one, which is the pick.
                    let verdict = if beats && !recommended {
                        recommended = true;
                        UpdateVerdict::Recommended
                    } else if beats {
                        UpdateVerdict::BeatsBoth
                    } else if speed <= current_speed {
                        UpdateVerdict::Slower
                    } else {
                        UpdateVerdict::Blunter
                    };
                    CandidateRow { row: row_for(entry), verdict }
                })
                .collect();

            Some(ModelUpdate {
                role: *role,
                file: spec.file.clone(),
                current: CurrentFacts { speed_x: current_speed, fleurs_en_wer: current_wer },
                candidates,
            })
        })
        .collect()
}

/// Whether a feed is old enough to fetch again: no feed at all, one
/// stamped older than [`REFRESH_AFTER`], or one whose stamp this build
/// cannot read - acting on an unreadable stamp beats trusting it.
pub(crate) fn refresh_is_due(catalogue: Option<&Catalogue>, now: SystemTime) -> bool {
    let Some(catalogue) = catalogue else {
        return true;
    };
    let Ok(at) = time::OffsetDateTime::parse(
        &catalogue.fetched_at,
        &time::format_description::well_known::Rfc3339,
    ) else {
        return true;
    };
    now.duration_since(SystemTime::from(at)).map_or(true, |age| age >= REFRESH_AFTER)
}

/// The live catalogue state: the last fetched feed and what the last
/// check did.
pub(crate) struct CatalogueState {
    pub(crate) check: CatalogueCheck,
    /// The last-known feed: from the cache at boot, replaced by each
    /// successful fetch, and left standing by a failed one.
    pub(crate) catalogue: Option<Catalogue>,
}

impl Default for CatalogueState {
    fn default() -> Self {
        Self { check: CatalogueCheck::Never, catalogue: None }
    }
}

impl crate::Workspace {
    /// Everything the models page draws.
    pub fn dictate_models(&self) -> DictateModelsSnapshot {
        let settings = &self.config.dictate;
        let active = self.active_models();
        let specs: Vec<(DictateRole, ModelSpec)> =
            active.iter().map(|(role, model)| (*role, model.spec.clone())).collect();
        let preflight = self.dictate.snapshot.lock().clone();
        let state = self.dictate_catalogue.lock();
        let entries: &[CatalogueEntry] =
            state.catalogue.as_ref().map_or(&[], |catalogue| catalogue.entries.as_slice());

        DictateModelsSnapshot {
            enabled: settings.enabled,
            models_dir: settings.models_dir(),
            in_use: if settings.enabled {
                in_use(entries, &active, &preflight)
            } else {
                Vec::new()
            },
            check: state.check.clone(),
            updates: if settings.enabled { updates_for(entries, &specs) } else { Vec::new() },
            rows: entries.iter().map(row_for).collect(),
            install: self.dictate_install(),
            activate: self.dictate_activate(),
            installed: self.installed_models(),
            bench: self.dictate_bench(),
            results: self.bench_results(),
            read_aloud: self.read_aloud_state(),
        }
    }

    /// Load the cached feed, then fetch a fresh one when it is due. One
    /// task per forge run, started beside the dictation preflight; a
    /// no-op unless `[dictate] enabled` is set.
    ///
    /// The cache read is synchronous - one small file on the boot path -
    /// and the fetch never is: it is what the state's [`CatalogueCheck`]
    /// reports on.
    pub fn start_dictate_catalogue(self: &std::sync::Arc<Self>) {
        if !self.config.dictate.enabled {
            return;
        }
        match self.catalogue_dir() {
            None => {
                self.spawn_catalogue_refresh();
            }
            Some(dir) => match forge_dictate::catalogue::read_catalogue_cache(&dir) {
                Ok(Some(catalogue)) => {
                    let due = refresh_is_due(Some(&catalogue), SystemTime::now());
                    // The check state comes off the cache too: the last
                    // check a page can read is the one that fetched it,
                    // even across a restart that has not fetched yet.
                    {
                        let mut state = self.dictate_catalogue.lock();
                        state.check = CatalogueCheck::Fresh {
                            at: catalogue.fetched_at.clone(),
                            release: catalogue.release.clone(),
                            skipped: catalogue.skipped,
                        };
                        state.catalogue = Some(catalogue);
                    }
                    if due {
                        self.spawn_catalogue_refresh();
                    }
                }
                Ok(None) => {
                    self.spawn_catalogue_refresh();
                }
                // A cache this build cannot read is a cache it does not
                // have; the record says why, so a corrupt file is not
                // indistinguishable from none and does not repeat
                // silently every boot.
                Err(error) => {
                    tracing::warn!(
                        event_name = "dictate_catalogue_cache_unreadable",
                        %error,
                        "the cached catalogue could not be read; fetching a fresh one"
                    );
                    self.spawn_catalogue_refresh();
                }
            },
        }
    }

    /// The models page's Check now: a fresh fetch, refused when there is
    /// nothing to check against or one is already running.
    pub(crate) fn check_dictate_catalogue(
        self: &std::sync::Arc<Self>,
    ) -> Result<(), crate::DispatchError> {
        if !self.config.dictate.enabled {
            return Err(crate::DispatchError::DictateOff);
        }
        if !self.spawn_catalogue_refresh() {
            return Err(crate::DispatchError::CatalogueChecking);
        }
        Ok(())
    }

    /// Fetch the feed off the runtime, land it, and push the re-read
    /// view. Answers `false` when a check is already in flight, deciding
    /// that under the same lock take that moves the state to
    /// [`CatalogueCheck::Checking`]: two near-simultaneous dispatches
    /// cannot both fetch.
    fn spawn_catalogue_refresh(self: &std::sync::Arc<Self>) -> bool {
        {
            let mut state = self.dictate_catalogue.lock();
            if matches!(state.check, CatalogueCheck::Checking) {
                return false;
            }
            state.check = CatalogueCheck::Checking;
        }
        let this = std::sync::Arc::clone(self);
        tokio::spawn(async move {
            let _ = this.fetch_catalogue_once().await;
            let models = this.dictate_models();
            let _ =
                this.update_sender().send(crate::SessionUpdate::DictateModelsChanged { models });
        });
        true
    }

    /// Fetch the feed once and land it: the freshness line, the rows and
    /// the cache. Answers the catalogue, or the error the fetch failed
    /// with; the last-known feed stands either way.
    ///
    /// Shared by the background check and the active-model resolution,
    /// which needs the feed's entry for a config key's variant.
    pub(crate) async fn fetch_catalogue_once(
        &self,
    ) -> Result<forge_dictate::catalogue::Catalogue, forge_dictate::Error> {
        let source = self.catalogue_source();
        let cleanup = self.cleanup_source();
        let fetched = tokio::task::spawn_blocking(move || {
            let mut catalogue = forge_dictate::catalogue::fetch_catalogue(&source)?;
            // **The cleanup feed is a second source, and its failure is its
            // own.** The speech feed landing is the check; a cleanup fetch
            // that could not be read costs its rows, not the check, and the
            // candidates the last fetch left stand.
            match forge_dictate::cleanup::fetch_cleanup(&cleanup) {
                Ok(mut entries) => {
                    catalogue.entries.append(&mut entries);
                    Ok::<_, forge_dictate::Error>((catalogue, true))
                }
                Err(error) => {
                    tracing::warn!(
                        event_name = "dictate_cleanup_feed_failed",
                        %error,
                        "the cleanup feed could not be read; the candidates the last fetch left stand"
                    );
                    Ok((catalogue, false))
                }
            }
        })
        .await;
        let outcome = fetched
            .unwrap_or_else(|join| {
                Err(forge_dictate::Error::Catalogue { message: join.to_string() })
            })
            .map(|(mut catalogue, cleanup_ok)| {
                if !cleanup_ok {
                    let previous: Vec<forge_dictate::catalogue::CatalogueEntry> = self
                        .dictate_catalogue
                        .lock()
                        .catalogue
                        .as_ref()
                        .map(|held| {
                            held.entries
                                .iter()
                                .filter(|entry| {
                                    entry.kind == forge_dictate::catalogue::EntryKind::Normalizer
                                })
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    catalogue.entries.splice(0..0, previous);
                }
                catalogue
            });

        match outcome {
            Ok(catalogue) => {
                if let Some(dir) = self.catalogue_dir()
                    && let Err(error) =
                        forge_dictate::catalogue::write_catalogue_cache(&dir, &catalogue)
                {
                    tracing::warn!(
                        event_name = "dictate_catalogue_cache_write_failed",
                        %error,
                        "the fetched catalogue was not cached; the next boot fetches it again"
                    );
                }
                let mut state = self.dictate_catalogue.lock();
                state.check = CatalogueCheck::Fresh {
                    at: catalogue.fetched_at.clone(),
                    release: catalogue.release.clone(),
                    skipped: catalogue.skipped,
                };
                state.catalogue = Some(catalogue.clone());
                Ok(catalogue)
            }
            // The rows the last fetch left stand, and nothing in use
            // changed: a failed check costs the freshness line, not
            // the feed.
            Err(error) => {
                tracing::warn!(
                    event_name = "dictate_catalogue_check_failed",
                    %error,
                    "the catalogue check failed; the last-known feed stands"
                );
                self.dictate_catalogue.lock().check =
                    CatalogueCheck::Unreachable { error: error.to_string() };
                Err(error)
            }
        }
    }

    /// Where the feed is fetched from. Test builds can point this at a
    /// loopback server; production always takes the real one.
    pub(crate) fn catalogue_source(&self) -> forge_dictate::catalogue::CatalogueSource {
        #[cfg(any(test, feature = "testing"))]
        if let Some(source) = self.test_catalogue_source.lock().clone() {
            return source;
        }
        forge_dictate::catalogue::CatalogueSource::default()
    }

    /// Where the cleanup feed is fetched from, on the same terms - and a test
    /// serving the speech feed serves this one from the same loopback, so a
    /// check in a test reads one server rather than reaching the Hub.
    pub(crate) fn cleanup_source(&self) -> forge_dictate::cleanup::CleanupSource {
        #[cfg(any(test, feature = "testing"))]
        if let Some(source) = self.test_cleanup_source.lock().clone() {
            return source;
        }
        #[cfg(any(test, feature = "testing"))]
        if let Some(base) = self.test_catalogue_source.lock().clone() {
            return forge_dictate::cleanup::CleanupSource {
                listing: format!("{}cleanup?filter=", base.entry_base),
                listing_tail: "&filter=gguf".to_owned(),
                tags: vec!["text-normalization".to_owned()],
                blobs_base: format!("{}blobs/", base.entry_base),
                files_base: format!("{}files/", base.entry_base),
            };
        }
        forge_dictate::cleanup::CleanupSource::default()
    }

    /// Where the fetched feed is cached: forge's machine-local state
    /// directory, or a test's own.
    fn catalogue_dir(&self) -> Option<PathBuf> {
        #[cfg(any(test, feature = "testing"))]
        if let Some(dir) = self.test_catalogue_dir.lock().clone() {
            return Some(dir);
        }
        match forge_sdk::app_support_dir() {
            Ok(dir) => Some(dir.join("dictate-catalogue")),
            Err(error) => {
                tracing::warn!(
                    event_name = "dictate_catalogue_dir_unresolved",
                    %error,
                    "no app-support dir: the fetched catalogue is not cached"
                );
                None
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests_catalogue_view {
    use super::*;
    use crate::dictate::{DictateModel, DictateModelState, DictateRole, DictateSnapshot};
    use forge_dictate::catalogue::parse_entry;

    /// A synthetic feed entry carrying every row the view reads. The
    /// real entries' values are pinned in `forge-dictate`'s own tests;
    /// these are the shapes this module's rules are about.
    fn entry(
        variant: &str,
        languages: &str,
        speed: f64,
        wer: f64,
    ) -> forge_dictate::catalogue::CatalogueEntry {
        entry_under(variant, languages, speed, wer, 100, "mit", "MIT")
    }

    /// [`entry`] with the download length and licence a test needs.
    ///
    /// `pub(crate)` so the install tests build their entries with the same
    /// spelling rather than a second one.
    pub(crate) fn entry_under(
        variant: &str,
        languages: &str,
        speed: f64,
        wer: f64,
        size_bytes: u64,
        spdx: &str,
        display: &str,
    ) -> forge_dictate::catalogue::CatalogueEntry {
        parse_entry(&format!(
            r#"{{
              "schema": "transcribe-catalog-v1",
              "variant": "{variant}",
              "display_name": "{variant}",
              "params": 1000000,
              "license": {{"spdx": "{spdx}", "display": "{display}"}},
              "languages": {languages},
              "downloads": [
                {{"quant": "F16", "filename": "{variant}-F16.gguf", "size_bytes": 200}},
                {{"quant": "Q4_K_M", "filename": "{variant}-Q4_K_M.gguf", "size_bytes": {size_bytes}}}
              ],
              "speed_benchmarks": [
                {{"machine": "m4-max", "backend": "metal", "quant": "Q8_0", "xrt_wall": {speed}}}
              ],
              "accuracy_benchmarks": [
                {{"dataset": "fleurs", "split": "test", "language": "en", "quant": "Q8_0", "err_pct": {wer}}}
              ]
            }}"#
        ))
        .expect("the synthetic entry parses")
    }

    /// The spec that joins `entry("in-use", ..)`: the file name is the
    /// join and the length is its witness, so both are the synthetic
    /// entry's own.
    fn in_use_spec() -> forge_dictate::ModelSpec {
        let mut spec = forge_dictate::ModelSpec::cohere_transcribe_q4_k_m();
        spec.file = "in-use-Q4_K_M.gguf".to_owned();
        spec.size = 100;
        spec
    }

    /// One role's compiled pin, as the active list spells it: what a role
    /// with no config key and no runtime pick runs.
    fn active_pin(
        role: DictateRole,
        spec: forge_dictate::ModelSpec,
    ) -> (DictateRole, crate::install::ActiveModel) {
        (
            role,
            crate::install::ActiveModel {
                role,
                spec,
                from: crate::install::ActiveFrom::Pin,
                at: None,
            },
        )
    }

    /// Every fact the candidate list draws comes off the row itself.
    #[test]
    fn a_row_carries_the_facts_the_candidate_list_draws() {
        let row = row_for(&entry("candidate", r#"["en", "fr"]"#, 388.8, 4.61));

        assert_eq!(row.variant, "candidate");
        assert_eq!(row.params, Some(1_000_000));
        assert_eq!(row.license.as_deref(), Some("MIT"));
        assert_eq!(row.languages, ["en", "fr"]);
        assert_eq!(
            row.download.as_ref().map(|download| (download.quant.as_str(), download.size_bytes)),
            Some(("Q4_K_M", 100)),
            "the candidate line's size is the Q4 build"
        );
        assert_eq!(row.speed.as_ref().map(|speed| speed.xrt_wall), Some(388.8));
        assert_eq!(row.speed.as_ref().map(|speed| speed.backend.as_str()), Some("metal"));
        assert_eq!(row.wer.as_ref().map(|wer| wer.err_pct), Some(4.61));
        assert_eq!(row.wer.as_ref().map(|wer| wer.language.as_str()), Some("en"));
    }

    /// A build with no Q4 falls back along the reference quants, and one
    /// with none of them carries no size rather than a wrong one.
    #[test]
    fn the_rows_download_falls_back_when_no_q4_build_exists() {
        let q8_only = parse_entry(
            r#"{"schema":"transcribe-catalog-v1","variant":"q8",
                "downloads":[{"quant":"F32","size_bytes":9},{"quant":"Q8_0","size_bytes":5}]}"#,
        )
        .expect("parse");
        assert_eq!(
            row_for(&q8_only).download.as_ref().map(|download| download.quant.as_str()),
            Some("Q8_0"),
            "without a Q4 build the reference quant chain is next"
        );

        let f32_only = parse_entry(
            r#"{"schema":"transcribe-catalog-v1","variant":"f32",
                "downloads":[{"quant":"F32","size_bytes":9}]}"#,
        )
        .expect("parse");
        assert!(
            row_for(&f32_only).download.is_none(),
            "no reference quant means no size, not the F32 one"
        );
    }

    /// The update is the candidate the page's line shows: the fastest
    /// entry that beats the model in use on both compared axes.
    #[test]
    fn the_update_is_the_fastest_candidate_that_beats_the_model_in_use() {
        // The winner is deliberately not the first beater in the list, so
        // a walk that keeps the first candidate it meets fails here; and
        // the list carries one entry per way to be admitted wrongly -
        // faster but blunter, and faster with an equal error rate.
        let entries = vec![
            entry("in-use", r#"["en"]"#, 72.9, 5.08),
            entry("parakeet-like", r#"["en"]"#, 218.8, 3.99),
            entry("granite-like", r#"["en"]"#, 388.8, 4.61),
            entry("fast-but-blunt", r#"["en"]"#, 900.0, 5.50),
            entry("faster-but-equal-wer", r#"["en"]"#, 500.0, 5.08),
            entry("slower-but-sharper", r#"["en"]"#, 50.0, 1.20),
            entry("russian", r#"["ru"]"#, 900.0, 0.90),
        ];

        let updates = updates_for(&entries, &[(DictateRole::Transcribing, in_use_spec())]);

        assert_eq!(updates.len(), 1, "one comparison per model in use");
        let update = &updates[0];
        assert_eq!(update.file, "in-use-Q4_K_M.gguf");
        assert_eq!(update.current.speed_x, 72.9, "the in-use side of the comparison");
        assert_eq!(update.current.fleurs_en_wer, 5.08);

        // The table: fastest first, the pick at its head, and every row
        // carrying the axis it lost or won on.
        let table: Vec<(&str, UpdateVerdict)> = update
            .candidates
            .iter()
            .map(|candidate| (candidate.row.variant.as_str(), candidate.verdict))
            .collect();
        assert_eq!(
            table,
            vec![
                ("fast-but-blunt", UpdateVerdict::Blunter),
                ("faster-but-equal-wer", UpdateVerdict::Blunter),
                ("granite-like", UpdateVerdict::Recommended),
                ("parakeet-like", UpdateVerdict::BeatsBoth),
                ("slower-but-sharper", UpdateVerdict::Slower),
            ],
            "the ranked comparison the page tables: fastest first, the pick flagged where it \
             lands, and every other row carrying the axis it lost on"
        );
    }

    /// Two candidates equally fast: the sharper takes the line.
    #[test]
    fn of_two_equally_fast_candidates_the_sharper_wins() {
        let entries = vec![
            entry("in-use", r#"["en"]"#, 72.9, 5.08),
            entry("twin-blunter", r#"["en"]"#, 300.0, 4.00),
            entry("twin-sharper", r#"["en"]"#, 300.0, 3.50),
        ];

        let updates = updates_for(&entries, &[(DictateRole::Transcribing, in_use_spec())]);

        assert_eq!(updates.len(), 1);
        assert_eq!(
            updates[0].candidates[0].row.variant, "twin-sharper",
            "equal speed is broken by the lower word error rate, not by list order"
        );
        assert_eq!(updates[0].candidates[0].verdict, UpdateVerdict::Recommended);
    }

    /// FLEURS English is the comparison axis: a row without it cannot be
    /// compared, however fast it is.
    #[test]
    fn a_candidate_with_no_comparable_wer_is_not_a_candidate() {
        let headline_only = parse_entry(
            r#"{"schema":"transcribe-catalog-v1","variant":"headline-only","languages":["en"],
                "headline_benchmark":{"dataset":"librispeech","split":"test-clean","language":"en"},
                "downloads":[{"quant":"Q4_K_M","filename":"x.gguf","size_bytes":1}],
                "speed_benchmarks":[{"machine":"m4-max","backend":"metal","quant":"Q8_0","xrt_wall":900.0}],
                "accuracy_benchmarks":[{"dataset":"librispeech","split":"test-clean","language":"en","quant":"Q8_0","err_pct":1.2}]}"#,
        )
        .expect("parse");
        let entries = vec![entry("in-use", r#"["en"]"#, 72.9, 5.08), headline_only];

        let updates = updates_for(&entries, &[(DictateRole::Transcribing, in_use_spec())]);

        assert_eq!(updates.len(), 1, "the baseline still stands");
        assert!(
            updates[0].candidates.is_empty(),
            "no common axis, no comparison - and no row for it"
        );
    }

    /// **A non-commercial licence is a fact on the row, not a filter.**
    /// The fastest beater wins and the row carries the licence it would
    /// run under - the page downloads and runs it on this machine, which
    /// is the licence's own personal use.
    #[test]
    fn a_non_commercial_candidate_is_recommended_with_its_licence_on_the_row() {
        let entries = vec![
            entry("in-use", r#"["en"]"#, 72.9, 5.08),
            entry_under(
                "faster-but-nc",
                r#"["en"]"#,
                401.64,
                4.30,
                100,
                "cc-by-nc-sa-4.0",
                "CC-BY-NC-SA-4.0",
            ),
            entry_under("permissive", r#"["en"]"#, 388.76, 4.61, 100, "apache-2.0", "Apache-2.0"),
        ];

        let updates = updates_for(&entries, &[(DictateRole::Transcribing, in_use_spec())]);

        assert_eq!(updates.len(), 1);
        assert_eq!(
            updates[0].candidates[0].row.variant, "faster-but-nc",
            "the fastest entry that beats the model in use, licence and all"
        );
        assert_eq!(updates[0].candidates[0].verdict, UpdateVerdict::Recommended);
        assert_eq!(
            updates[0].candidates[0].row.license.as_deref(),
            Some("CC-BY-NC-SA-4.0"),
            "and the row states the licence the decision is read under"
        );
        assert_eq!(updates[0].candidates[1].row.variant, "permissive");
        assert_eq!(updates[0].candidates[1].verdict, UpdateVerdict::BeatsBoth);
    }

    /// The pin's own byte length is the witness an entry is about the
    /// file in use: a feed carrying the same name at another length has
    /// been rebuilt, and its measured rows are not this file's.
    #[test]
    fn an_update_is_not_proposed_from_a_rebuilt_file() {
        let entries = vec![
            // Our file's name at another length: upstream rebuilt the
            // quant, and these rows are the new build's.
            entry_under("in-use", r#"["en"]"#, 300.0, 4.00, 101, "mit", "MIT"),
            // A candidate that WOULD beat those rows - which is exactly
            // what must not happen, because they are not our file's.
            entry("candidate", r#"["en"]"#, 500.0, 2.00),
        ];

        assert!(
            updates_for(&entries, &[(DictateRole::Transcribing, in_use_spec())]).is_empty(),
            "a same-name different-length download is a rebuild, and no comparison is built on it"
        );
        let join = join_for(&entries, &in_use_spec())
            .expect("the join still crosses, so a reader can see the length moved");
        assert_eq!(join.size_bytes, 101, "the feed's own length, against the pin's 100");
    }

    /// The model in use is not news about itself, whatever a duplicate
    /// entry in the feed claims.
    #[test]
    fn the_model_in_use_is_never_its_own_candidate() {
        let entries = vec![
            entry("in-use", r#"["en"]"#, 72.9, 5.08),
            entry("in-use", r#"["en"]"#, 900.0, 1.00),
        ];

        let updates = updates_for(&entries, &[(DictateRole::Transcribing, in_use_spec())]);

        assert_eq!(updates.len(), 1, "the comparison still stands");
        assert!(
            updates[0].candidates.is_empty(),
            "and the variant in use is no row of it: {:?}",
            updates[0].candidates
        );
    }

    /// The IN USE row: declared facts off the pin, the state off the
    /// preflight snapshot, the feed's facts off the join.
    #[test]
    fn in_use_facts_come_from_the_pin_the_state_and_the_join() {
        // The feed's own length differs from the pin's on purpose: which
        // side each number came from is the assertion.
        let entries = vec![entry_under("in-use", r#"["en", "fr"]"#, 72.9, 5.08, 200, "mit", "MIT")];
        let states = DictateSnapshot {
            models: vec![DictateModel {
                role: DictateRole::Transcribing,
                file: "in-use-Q4_K_M.gguf".to_owned(),
                state: DictateModelState::Ready,
            }],
            failure: None,
        };

        let rows =
            in_use(&entries, &[active_pin(DictateRole::Transcribing, in_use_spec())], &states);

        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(
            row.state,
            DictateModelState::Ready,
            "the page's loaded chip is the preflight snapshot's own state"
        );
        assert_eq!(row.facts.quant.as_deref(), Some("Q4_K_M"), "declared facts come off the pin");
        assert_eq!(row.size, 100, "the pin's own byte length");
        let join = row.catalogue.as_ref().expect("the file joins its catalogue entry");
        assert_eq!(join.variant, "in-use");
        assert_eq!(join.size_bytes, 200, "the feed's own size for the file, as a witness");
        assert_eq!(join.languages, ["en", "fr"]);
        assert_eq!(join.speed.as_ref().map(|speed| speed.xrt_wall), Some(72.9));
    }

    /// A model the feed does not carry still crosses whole.
    #[test]
    fn a_model_in_no_catalogue_entry_still_crosses_whole() {
        let rows = in_use(
            &[],
            &[active_pin(DictateRole::Normalization, forge_dictate::ModelSpec::s1_mini_f16())],
            &DictateSnapshot::default(),
        );

        assert_eq!(rows.len(), 1);
        assert!(rows[0].catalogue.is_none(), "the normalizer is in no feed; the pin carries it");
        assert_eq!(
            rows[0].state,
            DictateModelState::Pending,
            "no preflight row yet is pending, not missing"
        );
    }

    /// A feed older than a day refreshes at boot; a stamp this build
    /// cannot read is due too, because acting on it beats trusting it.
    #[test]
    fn a_feed_older_than_a_day_is_due_for_a_refresh() {
        let now = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_791_244_800);
        let stamp = |age: std::time::Duration| {
            let at = now - age;
            time::OffsetDateTime::from(at)
                .format(&time::format_description::well_known::Rfc3339)
                .expect("a stamp formats")
        };
        let feed = |fetched_at: String| forge_dictate::catalogue::Catalogue {
            fetched_at,
            release: None,
            entries: Vec::new(),
            skipped: 0,
        };

        assert!(refresh_is_due(None, now), "a machine that never fetched is due");
        assert!(
            !refresh_is_due(Some(&feed(stamp(std::time::Duration::from_secs(3600)))), now),
            "an hour old is not due"
        );
        assert!(
            refresh_is_due(Some(&feed(stamp(std::time::Duration::from_secs(25 * 3600)))), now),
            "past a day is due"
        );
        assert!(
            refresh_is_due(Some(&feed("whenever".to_owned())), now),
            "a stamp this build cannot read is due, not trusted"
        );
    }

    /// The check crosses under the names the client narrows on.
    #[test]
    fn the_check_crosses_under_the_names_the_client_narrows() {
        for (check, name) in [
            (CatalogueCheck::Never, "never"),
            (CatalogueCheck::Checking, "checking"),
            (CatalogueCheck::Unreachable { error: "502".to_owned() }, "unreachable"),
        ] {
            let value = serde_json::to_value(&check).expect("serialise");
            assert_eq!(value["state"], name, "the client narrows on this tag: {value}");
        }

        let fresh = CatalogueCheck::Fresh {
            at: "2026-10-06T06:12:00Z".to_owned(),
            release: Some("v0.3.1".to_owned()),
            skipped: 2,
        };
        let value = serde_json::to_value(&fresh).expect("serialise");
        assert_eq!(value["state"], "fresh");
        assert_eq!(value["release"], "v0.3.1");
        assert_eq!(value["skipped"], 2);
    }

    // ---- the workspace's own check path ----

    use crate::{Command, DispatchError, SessionUpdate, Workspace};
    use forge_dictate::catalogue::{
        Catalogue, CatalogueSource, read_catalogue_cache, write_catalogue_cache,
    };
    use std::sync::Arc;

    /// A stub whose `[dictate]` section is on, with the models directory
    /// a tempdir so nothing touches the real cache.
    ///
    /// `pub(crate)` for the install tests, which drive the same stub and
    /// the same loopback server rather than carrying a second copy.
    pub(crate) fn enabled_stub()
    -> (Arc<Workspace>, tokio::sync::mpsc::UnboundedReceiver<SessionUpdate>, tempfile::TempDir)
    {
        let config_dir = tempfile::tempdir().expect("a config dir");
        let models = tempfile::tempdir().expect("a models dir");
        let mut config = crate::config::LoadedConfig::empty_for_test();
        config.dictate.enabled = true;
        config.dictate.models_dir = Some(models.path().to_string_lossy().into_owned());
        let (ws, updates) =
            Workspace::testing_stub_with_config(config_dir.path().to_path_buf(), config)
                .expect("the stub builds");
        (ws, updates, models)
    }

    /// Loopback HTTP/1.1 server answering fixed paths, one request per
    /// connection. Anything unrouted answers 404.
    pub(crate) fn serve(routes: Vec<(&'static str, u16, Vec<u8>)>) -> String {
        serve_with(|_| {
            routes
                .into_iter()
                .map(|(route, status, body)| (route.to_owned(), status, body))
                .collect()
        })
    }

    /// [`serve`] with the routes built from the bound address, for a body
    /// that must carry the URL it is served from: an install's doc table
    /// points its download links back at this same loopback.
    pub(crate) fn serve_with(
        routes_for: impl FnOnce(&str) -> Vec<(String, u16, Vec<u8>)>,
    ) -> String {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let base = format!("http://{}", listener.local_addr().expect("the bound address"));
        let routes = routes_for(&base);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().expect("a clone"));
                let mut request = String::new();
                if reader.read_line(&mut request).is_err() {
                    continue;
                }
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let (status, body) = match routes.iter().find(|(route, _, _)| route == &path) {
                    Some((_, status, body)) => (*status, body.clone()),
                    None => (404, Vec::new()),
                };
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        base
    }

    pub(crate) fn source(base: &str) -> CatalogueSource {
        CatalogueSource {
            listing: format!("{base}/catalog"),
            entry_base: format!("{base}/catalog/"),
            release: format!("{base}/release"),
            doc_base: format!("{base}/docs/"),
            repo_base: format!("{base}/repos/"),
        }
    }

    /// One parseable entry, shaped like the feed's own documents.
    pub(crate) fn feed_entry() -> Vec<u8> {
        br#"{"schema": "transcribe-catalog-v1", "variant": "one", "languages": ["en"],
             "downloads": [{"quant": "Q4_K_M", "filename": "one-Q4_K_M.gguf", "size_bytes": 100}],
             "speed_benchmarks": [{"machine": "m4-max", "backend": "metal", "quant": "Q8_0", "xrt_wall": 100.0}],
             "accuracy_benchmarks": [{"dataset": "fleurs", "split": "test", "language": "en", "quant": "Q8_0", "err_pct": 4.0}]}"#
            .to_vec()
    }

    pub(crate) fn listing() -> Vec<u8> {
        br#"[{"name": "one.json", "type": "file"}]"#.to_vec()
    }

    /// Before any feed, the page still reads the pins and the preflight
    /// state - and nothing invented for the parts a feed would answer.
    #[test]
    fn the_read_answers_the_pins_before_any_catalogue() {
        let (ws, _updates, _models) = enabled_stub();

        let view = ws.dictate_models();

        assert!(view.enabled);
        assert_eq!(view.in_use.len(), 2, "both pinned models cross");
        assert_eq!(
            view.in_use[0].state,
            crate::dictate::DictateModelState::Pending,
            "preflight has not got there yet"
        );
        assert!(view.in_use[0].catalogue.is_none(), "no feed, no join");
        assert!(matches!(view.check, CatalogueCheck::Never));
        assert!(view.rows.is_empty(), "no fetched feed is no rows");
        assert!(view.updates.is_empty(), "and no update to propose");
        assert_eq!(
            view.bench,
            crate::bench::BenchState::Idle,
            "no bench is running on a fresh read"
        );
        assert!(view.results.is_empty(), "and nothing has been measured yet");
    }

    /// With `[dictate]` off the page reads no models in use and no
    /// proposals, while the feed's rows still cross: the catalogue is
    /// not per-configuration, and inventing a configuration from it
    /// would draw a section nobody switched on.
    #[test]
    fn a_disabled_dictation_reads_no_models_and_no_proposals() {
        let (ws, _updates) = Workspace::testing_stub();
        // The in-use entry IS the production pin - same variant name,
        // same quant file, same length - so without the `enabled` gate
        // the candidate below would be proposed and this test would see
        // it.
        ws.dictate_catalogue.lock().catalogue = Some(Catalogue {
            fetched_at: "2026-10-06T00:00:00Z".to_owned(),
            release: None,
            entries: vec![
                entry_under(
                    "cohere-transcribe-03-2026",
                    r#"["en"]"#,
                    72.9,
                    5.08,
                    1_558_162_944,
                    "apache-2.0",
                    "Apache-2.0",
                ),
                entry("better", r#"["en"]"#, 500.0, 4.00),
            ],
            skipped: 0,
        });

        let view = ws.dictate_models();

        assert!(!view.enabled);
        assert!(view.in_use.is_empty(), "no pins are in use when dictation is off");
        assert!(view.updates.is_empty(), "and nothing is proposed against them");
        assert_eq!(view.rows.len(), 2, "the feed's rows still cross");
    }

    /// With no test override the cache lives beside forge's other
    /// machine-local state: a boot- or pid-scoped path would strand every
    /// fetched feed on the next start.
    #[test]
    fn the_production_cache_dir_is_the_app_support_one() {
        let (ws, _updates, _models) = enabled_stub();

        assert_eq!(
            ws.catalogue_dir(),
            forge_sdk::app_support_dir().ok().map(|dir| dir.join("dictate-catalogue")),
            "the cache directory is derived from the app-support dir, not from anything the boot \
             happens to know"
        );
    }

    /// A boot with a fresh cache reads the feed and does not fetch; a
    /// stale one fetches in the background.
    #[test]
    fn a_boot_reads_a_fresh_cache_and_does_not_fetch() {
        let (ws, _updates, _models) = enabled_stub();
        let dir = tempfile::tempdir().expect("a cache dir");
        *ws.test_catalogue_dir.lock() = Some(dir.path().to_path_buf());
        let now = std::time::SystemTime::now();
        let cached = Catalogue {
            fetched_at: time::OffsetDateTime::from(now)
                .format(&time::format_description::well_known::Rfc3339)
                .expect("a stamp"),
            release: Some("v0.3.1".to_owned()),
            entries: vec![
                parse_entry(&String::from_utf8(feed_entry()).expect("utf-8")).expect("parse"),
            ],
            skipped: 0,
        };
        write_catalogue_cache(dir.path(), &cached).expect("the cache writes");

        ws.start_dictate_catalogue();

        let view = ws.dictate_models();
        assert_eq!(view.rows.len(), 1, "the cached rows read before any fetch lands");
        assert!(
            matches!(&view.check, CatalogueCheck::Fresh { release, .. } if release.as_deref() == Some("v0.3.1")),
            "the freshness line is the last check that fetched this cache, got {:?}",
            view.check
        );
    }

    /// The Check now path: fetch, land the feed in the state, cache it,
    /// and push the re-read view to a subscriber.
    #[tokio::test]
    async fn a_check_lands_the_feed_and_pushes_the_view() {
        let (ws, mut updates, _models) = enabled_stub();
        let dir = tempfile::tempdir().expect("a cache dir");
        *ws.test_catalogue_dir.lock() = Some(dir.path().to_path_buf());
        let base = serve(vec![
            ("/catalog", 200, listing()),
            ("/catalog/one.json", 200, feed_entry()),
            ("/release", 200, br#"{"tag_name": "v0.3.1"}"#.to_vec()),
        ]);
        *ws.test_catalogue_source.lock() = Some(source(&base));

        ws.dispatch(Command::DictateCatalogueCheck).expect("the check dispatches");

        let landed = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                match updates.recv().await {
                    Some(SessionUpdate::DictateModelsChanged { models }) => break models,
                    Some(_) => {}
                    None => panic!("the subscription must stay attached"),
                }
            }
        })
        .await
        .expect("the check must land inside fifteen seconds");

        let CatalogueCheck::Fresh { release, skipped, .. } = &landed.check else {
            panic!("a landed check is fresh, got {:?}", landed.check)
        };
        assert_eq!(release.as_deref(), Some("v0.3.1"));
        assert_eq!(*skipped, 0);
        assert_eq!(landed.rows.len(), 1, "the pushed view carries the feed's rows");
        assert_eq!(landed.in_use.len(), 2, "and the pins beside them");
        assert_eq!(
            read_catalogue_cache(dir.path()).expect("the cache reads").map(|c| c.entries.len()),
            Some(1),
            "the fetched feed is cached for the next boot"
        );

        let view = ws.dictate_models();
        assert!(
            matches!(&view.check, CatalogueCheck::Fresh { .. }),
            "the state a later reader gets agrees with the pushed one"
        );
    }

    /// **The cleanup feed's own path runs in CI.** Its listing is fetched per
    /// tag and merged, its blobs answer each file's size and digest, and the
    /// rows carry both - a path until now only the ignored live reporter
    /// exercised, so a broken merge or a dropped digest was invisible here.
    #[tokio::test]
    async fn a_check_lands_the_cleanup_feed_too() {
        let (ws, _updates, _models) = enabled_stub();
        let cleanup = br#"[{"id": "owner/norm-a", "downloads": 5000,
            "pipeline_tag": "text-generation", "library_name": "gguf",
            "tags": ["gguf", "text-generation", "text-normalization", "en",
                     "base_model:owner/norm-base"]}]"#;
        let blobs = br#"{"siblings": [{"rfilename": "norm-a-Q4_K_M.gguf", "size": 300000000,
            "lfs": {"sha256": "abababab"}}],
            "gguf": {"architecture": "qwen2"}}"#;
        let base = serve(vec![
            ("/catalog", 200, listing()),
            ("/catalog/one.json", 200, feed_entry()),
            ("/release", 200, br#"{"tag_name": "v0.3.1"}"#.to_vec()),
            ("/catalog/cleanup?filter=text-normalization&filter=gguf", 200, cleanup.to_vec()),
            ("/catalog/blobs/owner/norm-a?blobs=true", 200, blobs.to_vec()),
        ]);
        *ws.test_catalogue_source.lock() = Some(source(&base));

        let landed = ws.fetch_catalogue_once().await.expect("both feeds land");
        let cleanup_rows: Vec<_> = landed
            .entries
            .iter()
            .filter(|entry| entry.kind == forge_dictate::catalogue::EntryKind::Normalizer)
            .collect();
        assert_eq!(cleanup_rows.len(), 1, "the cleanup listing lands its row");
        let row = cleanup_rows[0];
        assert_eq!(row.variant, "owner/norm-a");
        assert_eq!(row.downloads.len(), 1, "the blobs answer the file");
        assert_eq!(row.downloads[0].quant, "Q4_K_M");
        assert_eq!(row.downloads[0].size_bytes, 300_000_000);
        assert_eq!(
            row.downloads[0].sha256.as_deref(),
            Some("abababab"),
            "the blobs' digest rides the download, which is what an install verifies"
        );

        // And the speech feed's own row stands beside it: one catalogue, two
        // kinds, one read.
        assert!(
            landed
                .entries
                .iter()
                .any(|entry| entry.kind == forge_dictate::catalogue::EntryKind::Asr),
            "the speech feed's rows land in the same read"
        );
    }

    /// A second check while one is running is refused: the one in flight
    /// is the answer.
    #[test]
    fn a_check_while_one_is_running_is_refused() {
        let (ws, _updates, _models) = enabled_stub();
        ws.dictate_catalogue.lock().check = CatalogueCheck::Checking;

        let err = ws
            .dispatch(Command::DictateCatalogueCheck)
            .expect_err("a second check must be refused");

        assert!(matches!(err, DispatchError::CatalogueChecking), "got: {err:?}");
    }

    /// With dictation off there is nothing to check against.
    #[test]
    fn a_check_with_dictation_off_is_refused() {
        let (ws, _updates) = Workspace::testing_stub();

        let err =
            ws.dispatch(Command::DictateCatalogueCheck).expect_err("no pinned models, no check");

        assert!(matches!(err, DispatchError::DictateOff), "got: {err:?}");
    }
}
