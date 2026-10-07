//! Benching a model on this machine's own material: which models a run can
//! load, the config each one runs under, and the state and results the
//! page reads.
//!
//! The corpus and the runner are `forge-dictate`'s; this module is the
//! workspace's half - it knows the roles, the installed set and the store.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use forge_dictate::ModelSpec;

use crate::{DispatchError, Workspace};

/// Which slot a bench target runs in, in the names the wire uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchRole {
    Transcribing,
    Cleanup,
}

/// One model a bench can load.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BenchTarget {
    pub file: String,
    pub role: BenchRole,
    /// Whether `forge.toml` pins the role this target would take - the
    /// page offers no activation for one, and a bench still runs.
    pub pinned: bool,
}

/// Where the last bench got to, as the page draws it.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BenchState {
    #[default]
    Idle,
    /// `clip` of `clips` is counting clips done, and `so_far` is the share
    /// of the compared ones that agreed word-for-word so far, when any
    /// clip carried something to compare.
    Running {
        target: BenchTarget,
        tier: forge_dictate::bench::Tier,
        clip: usize,
        clips: usize,
        so_far: Option<f64>,
    },
    /// The run stopped with a reason. Nothing was saved.
    Failed { target: BenchTarget, reason: String },
}

/// What one finished bench measured, kept under what it was about.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BenchResult {
    pub target: BenchTarget,
    pub tier: forge_dictate::bench::Tier,
    pub metrics: forge_dictate::bench::Metrics,
    /// RFC 3339.
    pub at: String,
    /// The corpus it ran over, so two numbers are only compared when these
    /// agree.
    pub corpus: forge_dictate::bench::CorpusId,
}

/// How many saved results the page reads.
const RESULTS_SHOWN: usize = 50;

/// The read-aloud set as the page reads it: whether this machine has one,
/// whether one is ARMED for the next take, and the passage it was read from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReadAloudState {
    pub recorded: bool,
    /// A take is owed to an arming: the next finished take becomes the set.
    /// Carried so the press that armed it draws something - a control whose
    /// only effect is a file the page cannot see reads as broken.
    pub armed: bool,
    pub passage: String,
}

/// The directory the read-aloud set lives under, beside forge's other
/// machine-local stores; `None` when no app-support dir resolves.
pub(crate) fn read_aloud_dir() -> Option<std::path::PathBuf> {
    forge_sdk::app_support_dir().ok().map(|dir| dir.join("dictate-read-aloud"))
}

/// The config one target runs under: the base with the target's file in
/// its role's slot.
///
/// The spec carries no URL and no digest - a bench loads what is already
/// on disk and never fetches - and the size is the file's own, so the
/// engine's check is against the bytes that are there.
pub fn config_for(
    base: &forge_dictate::Config,
    models_dir: &Path,
    target: &BenchTarget,
) -> Result<forge_dictate::Config, forge_dictate::Error> {
    let path = models_dir.join(&target.file);
    let size = std::fs::metadata(&path)
        .map_err(|source| forge_dictate::Error::Io { path: path.clone(), source })?
        .len();
    let spec = ModelSpec {
        file: target.file.clone(),
        url: String::new(),
        size,
        sha256: None,
        facts: forge_dictate::ModelFacts::default(),
    };

    let mut cfg = base.clone();
    // **A bench run is not the user's dictation.** Its clips would otherwise
    // land in the per-take diagnostics store - filling the shelf the bench
    // itself scores against, so every run grew its own corpus - and a clip
    // finishing while a read-aloud set was armed would answer the arming.
    cfg.diagnostics_dir = None;
    cfg.read_aloud_dir = None;
    match target.role {
        BenchRole::Transcribing => cfg.asr_model = spec,
        BenchRole::Cleanup => cfg.normalizer = Some(spec),
    }
    Ok(cfg)
}

impl Workspace {
    /// The bench's state, for the page's read.
    pub fn dictate_bench(&self) -> BenchState {
        self.dictate_bench.lock().clone()
    }

    /// The read-aloud set, as the page draws it: whether one exists here,
    /// whether one is armed for the next take, and the passage it was read
    /// from. The set is the machine's rather than the workspace's, so this
    /// needs no handle to answer.
    pub fn read_aloud_state() -> ReadAloudState {
        let dir = read_aloud_dir();
        ReadAloudState {
            recorded: dir.as_ref().is_some_and(|dir| dir.join("passage.txt").is_file()),
            armed: dir.is_some_and(|dir| dir.join("armed.txt").is_file()),
            passage: forge_dictate::bench::READ_ALOUD_PASSAGE.to_owned(),
        }
    }

    /// Arm the read-aloud set: the NEXT finished take is stored as the
    /// passage's own reading, and a later arming replaces it.
    ///
    /// The marker is the passage itself, written where the engine's take
    /// path looks for it - so the take that answers an arming is the very
    /// take being captured, never a neighbour's.
    pub(crate) fn arm_read_aloud(&self) -> Result<(), DispatchError> {
        if !self.config.dictate.enabled {
            return Err(DispatchError::DictateOff);
        }
        let Some(dir) = read_aloud_dir() else {
            return Err(DispatchError::ReadAloudUnavailable {
                reason: "no app-support directory resolves".to_owned(),
            });
        };
        if let Err(error) = std::fs::create_dir_all(&dir) {
            tracing::warn!(%error, dir = %dir.display(), "read-aloud: the set directory is not writable");
            return Err(DispatchError::ReadAloudUnavailable { reason: error.to_string() });
        }
        let marker = dir.join("armed.txt");
        if let Err(error) = std::fs::write(&marker, forge_dictate::bench::READ_ALOUD_PASSAGE) {
            tracing::warn!(%error, path = %marker.display(), "read-aloud: the arming was not written");
            return Err(DispatchError::ReadAloudUnavailable { reason: error.to_string() });
        }
        Ok(())
    }

    /// Every saved result, newest first, capped at what the page draws.
    pub fn bench_results(&self) -> Vec<BenchResult> {
        let db = self.db.lock();
        db.as_ref()
            .map(|db| {
                let mut rows = crate::store::bench_results::results(db).unwrap_or_default();
                rows.truncate(RESULTS_SHOWN);
                rows
            })
            .unwrap_or_default()
    }

    /// Run one model over one tier's corpus on this machine's own material.
    ///
    /// Refused while a take is live (the microphone is the machine's, and a
    /// bench would hold a second engine through it) and while another bench
    /// runs - one at a time, like a download. The outcome rides
    /// [`SessionUpdate::DictateModelsChanged`], the way every other dictate
    /// action's does.
    pub(crate) fn start_bench(
        self: &Arc<Self>,
        target: BenchTarget,
        tier: forge_dictate::bench::Tier,
    ) -> Result<(), DispatchError> {
        if !self.config.dictate.enabled {
            return Err(DispatchError::DictateOff);
        }
        if matches!(*self.dictate_bench.lock(), BenchState::Running { .. }) {
            return Err(DispatchError::BenchRunning);
        }
        if let Some(holder) = self.dictate_runtime.lock().live_holder() {
            return Err(DispatchError::TakeLive { holder });
        }
        *self.dictate_bench.lock() =
            BenchState::Running { target: target.clone(), tier, clip: 0, clips: 0, so_far: None };
        self.dictate_bench_cancel.store(false, Ordering::Relaxed);

        let this = Arc::clone(self);
        tokio::spawn(async move {
            let runner = Arc::clone(&this);
            runner.run_bench(target, tier).await;
            this.push_models();
        });
        Ok(())
    }

    /// Stop the run in flight. What it measured so far is discarded rather
    /// than saved: a partial corpus is not a result.
    pub(crate) fn stop_bench(&self) -> Result<(), DispatchError> {
        if !matches!(*self.dictate_bench.lock(), BenchState::Running { .. }) {
            return Err(DispatchError::BenchNotRunning);
        }
        self.dictate_bench_cancel.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Whether a bench holds the machine, which is what a swap waits on.
    pub(crate) fn bench_running(&self) -> bool {
        matches!(*self.dictate_bench.lock(), BenchState::Running { .. })
    }

    /// Gather the corpus, build the target's config, and run it.
    async fn run_bench(self: Arc<Self>, target: BenchTarget, tier: forge_dictate::bench::Tier) {
        let settings = self.config.dictate.clone();
        let active = self.active_models();
        let cfg = crate::dictate::preflight_config(&settings, &active);
        let Some(models_dir) = settings.models_dir() else {
            return self.fail_bench(target, "no models directory is configured".to_owned());
        };
        let takes_dir = cfg.diagnostics_dir.clone();
        let read_aloud_dir = cfg.read_aloud_dir.clone();
        // The closure takes its own copy: the caller keeps the target for
        // the failure paths below.
        let ran = target.clone();

        let this = Arc::clone(&self);
        let outcome = tokio::task::spawn_blocking(move || {
            let Some(takes_dir) = takes_dir else {
                return Err("no diagnostics directory is configured".to_owned());
            };
            let read_aloud_dir = read_aloud_dir.unwrap_or_else(|| takes_dir.clone());
            let corpus = forge_dictate::bench::corpus(tier, &takes_dir, &read_aloud_dir)
                .map_err(|error| error.to_string())?;
            if corpus.clips.is_empty() {
                return Err(
                    "nothing to bench: no takes saved here and no read-aloud set".to_owned()
                );
            }
            let run_cfg = config_for(&cfg, &models_dir, &ran).map_err(|e| e.to_string())?;

            let mut compared = 0_usize;
            let mut agreed = 0_usize;
            let clips = corpus.clips.len();
            let outcome = forge_dictate::bench::run(&run_cfg, &corpus, |clip| {
                if clip.matched.is_some() {
                    compared += 1;
                    if clip.matched == Some(true) {
                        agreed += 1;
                    }
                }
                *this.dictate_bench.lock() = BenchState::Running {
                    target: ran.clone(),
                    tier,
                    clip: clip.index + 1,
                    clips,
                    so_far: (compared > 0).then(|| forge_dictate::bench::ratio(agreed, compared)),
                };
                this.push_models();
                if this.dictate_bench_cancel.load(Ordering::Relaxed) {
                    std::ops::ControlFlow::Break(())
                } else {
                    std::ops::ControlFlow::Continue(())
                }
            });
            Ok::<_, String>((outcome, corpus.id))
        })
        .await;

        match outcome {
            Ok(Ok((Ok(metrics), corpus))) => {
                let result = BenchResult {
                    target: target.clone(),
                    tier,
                    metrics,
                    at: crate::install::rfc3339_now(),
                    corpus,
                };
                self.save_bench_result(&result);
                *self.dictate_bench.lock() = BenchState::Idle;
            }
            Ok(Ok((Err(error), _))) => self.settle_bench(&target, error.to_string()),
            Ok(Err(reason)) => self.settle_bench(&target, reason),
            Err(join) => self.settle_bench(&target, join.to_string()),
        }
    }

    /// A run that ended without a result: idle when it was stopped on
    /// request - a partial corpus is not a result, and nothing is saved -
    /// and failed with its own reason otherwise.
    fn settle_bench(&self, target: &BenchTarget, reason: String) {
        if self.dictate_bench_cancel.load(Ordering::Relaxed) {
            *self.dictate_bench.lock() = BenchState::Idle;
        } else {
            self.fail_bench(target.clone(), reason);
        }
    }

    fn fail_bench(&self, target: BenchTarget, reason: String) {
        *self.dictate_bench.lock() = BenchState::Failed { target, reason };
    }

    fn save_bench_result(&self, result: &BenchResult) {
        let db = self.db.lock();
        if let Some(db) = db.as_ref()
            && let Err(error) = crate::store::bench_results::save(db, result)
        {
            tracing::warn!(
                event_name = "dictate_bench_save_failed",
                %error,
                file = %result.target.file,
                "the bench's result was not saved; the numbers are gone with the run"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file present on disk, so the target rules are about the rules.
    fn present(dir: &Path, file: &str) {
        std::fs::write(dir.join(file), b"weights").unwrap();
    }

    fn target(file: &str, role: BenchRole) -> BenchTarget {
        BenchTarget { file: file.to_owned(), role, pinned: false }
    }

    fn metrics(clips: usize) -> forge_dictate::bench::Metrics {
        forge_dictate::bench::Metrics {
            clips,
            audio_seconds: 12.5,
            wall_seconds: 1.25,
            xrt_wall: 10.0,
            term_accuracy: Some(0.75),
            wer: Some(0.05),
            matched: Some((2, 3)),
            stages_ms: forge_dictate::bench::StageTotals::default(),
        }
    }

    fn corpus_id(sha: &str) -> forge_dictate::bench::CorpusId {
        forge_dictate::bench::CorpusId { clips: 3, audio_seconds: 12, sha256: sha.to_owned() }
    }

    /// The config a target runs under: the base with the target's file in
    /// its slot, no URL and no digest - the bench fetches nothing - and the
    /// size the file's own.
    #[test]
    fn a_targets_config_swaps_its_own_slot_and_leaves_the_other_alone() {
        let dir = tempfile::tempdir().unwrap();
        present(dir.path(), "candidate.gguf");
        let base = forge_dictate::Config::default();

        let for_it = config_for(
            &base,
            dir.path(),
            &BenchTarget {
                file: "candidate.gguf".to_owned(),
                role: BenchRole::Transcribing,
                pinned: false,
            },
        )
        .expect("the file is on disk");

        assert_eq!(for_it.asr_model.file, "candidate.gguf");
        assert!(for_it.asr_model.url.is_empty(), "a bench never fetches");
        assert!(for_it.asr_model.sha256.is_none(), "no digest is published for one");
        assert_eq!(for_it.asr_model.size, 7, "the size is the bytes on disk");
        assert!(
            for_it.diagnostics_dir.is_none() && for_it.read_aloud_dir.is_none(),
            "a bench run must not write takes into the store it scores against, nor answer a \
             read-aloud arming"
        );
        assert_eq!(
            for_it.normalizer.as_ref().map(|spec| spec.file.clone()),
            base.normalizer.as_ref().map(|spec| spec.file.clone()),
            "the other role keeps its model"
        );

        let for_cleanup = config_for(
            &base,
            dir.path(),
            &BenchTarget {
                file: "candidate.gguf".to_owned(),
                role: BenchRole::Cleanup,
                pinned: false,
            },
        )
        .expect("the file is on disk");
        assert_eq!(
            for_cleanup.normalizer.as_ref().map(|spec| spec.file.as_str()),
            Some("candidate.gguf")
        );
        assert_eq!(for_cleanup.asr_model.file, base.asr_model.file);
    }

    /// A target whose file vanished between the list and the run fails
    /// naming the path rather than loading something else.
    #[test]
    fn a_target_whose_file_is_gone_fails_by_name() {
        let dir = tempfile::tempdir().unwrap();

        let err = config_for(
            &forge_dictate::Config::default(),
            dir.path(),
            &target("gone.gguf", BenchRole::Transcribing),
        )
        .expect_err("no file, no config");

        assert!(err.to_string().contains("gone.gguf"), "got: {err}");
    }

    /// **A bench refuses by name while a take is live** - the microphone is
    /// the machine's, and a bench would hold a second engine through it.
    #[test]
    fn a_bench_refuses_by_name_while_a_take_is_live() {
        let (ws, _updates, _models) = crate::catalogue::tests_catalogue_view::enabled_stub();
        let (stop, _stop_rx) = tokio::sync::mpsc::channel(1);
        ws.dictate_runtime.lock().recordings.insert(
            crate::SessionSlot::new("Busytools", "forge", "worker"),
            crate::dictate::LiveRecording { stop, sink: None, initiator: None },
        );

        let err = ws
            .dispatch(crate::Command::DictateBench {
                target: target("asr.gguf", BenchRole::Transcribing),
                tier: forge_dictate::bench::Tier::Consensus,
            })
            .expect_err("a live take refuses the run");

        assert!(
            matches!(&err, crate::DispatchError::TakeLive { holder } if holder == "Busytools/forge/worker"),
            "the refusal names the seat whose take is live, got: {err:?}"
        );
        assert_eq!(ws.dictate_bench(), BenchState::Idle, "and nothing started");
    }

    /// A stop with nothing running is its own refusal, and a run that was
    /// stopped settles to idle with nothing saved - **a partial corpus is
    /// not a result.**
    #[test]
    fn a_stopped_run_settles_to_idle_and_saves_nothing() {
        let (ws, _updates) = Workspace::testing_stub();
        let dir = tempfile::tempdir().unwrap();
        ws.install_db_for_test(crate::store::Db::open(&dir.path().join("db.redb")).unwrap());

        let err = ws.dispatch(crate::Command::DictateBenchStop).expect_err("nothing to stop");
        assert!(matches!(err, crate::DispatchError::BenchNotRunning), "got: {err:?}");

        let target = target("asr.gguf", BenchRole::Transcribing);
        *ws.dictate_bench.lock() = BenchState::Running {
            target: target.clone(),
            tier: forge_dictate::bench::Tier::Consensus,
            clip: 3,
            clips: 12,
            so_far: Some(0.5),
        };
        ws.dispatch(crate::Command::DictateBenchStop).expect("a running bench stops");

        ws.settle_bench(&target, forge_dictate::Error::Cancelled.to_string());

        assert_eq!(ws.dictate_bench(), BenchState::Idle, "a stop is not a failure");
        assert!(ws.bench_results().is_empty(), "nothing measured part-way is kept");
    }

    /// A run that ends any other way lands failed, with its own reason.
    #[test]
    fn a_run_that_ends_without_a_result_lands_failed() {
        let (ws, _updates) = Workspace::testing_stub();
        let target = target("asr.gguf", BenchRole::Transcribing);

        ws.settle_bench(&target, "the model would not load".to_owned());

        let BenchState::Failed { target: failed, reason } = ws.dictate_bench() else {
            panic!("a failure must land as one, got {:?}", ws.dictate_bench());
        };
        assert_eq!(failed.file, "asr.gguf");
        assert_eq!(reason, "the model would not load");
    }

    /// A result round-trips through the store newest first, and a re-run
    /// over the same corpus replaces its row rather than stacking one.
    #[test]
    fn a_result_round_trips_through_the_store_newest_first() {
        let (ws, _updates) = Workspace::testing_stub();
        let dir = tempfile::tempdir().unwrap();
        ws.install_db_for_test(crate::store::Db::open(&dir.path().join("db.redb")).unwrap());
        let target = target("asr.gguf", BenchRole::Transcribing);

        let older = BenchResult {
            target: target.clone(),
            tier: forge_dictate::bench::Tier::Consensus,
            metrics: metrics(15),
            at: "2026-10-06T10:00:00Z".to_owned(),
            corpus: corpus_id("aaaa"),
        };
        let mut newer = older.clone();
        newer.at = "2026-10-06T11:00:00Z".to_owned();
        newer.corpus = corpus_id("bbbb");
        ws.save_bench_result(&older);
        ws.save_bench_result(&newer);

        let rows = ws.bench_results();

        assert_eq!(rows.len(), 2, "two corpora are two results");
        assert_eq!(rows[0].at, "2026-10-06T11:00:00Z", "newest first");
        assert_eq!(rows[0].metrics.clips, 15, "the metrics cross whole");
        assert_eq!(rows[0].metrics.term_accuracy, Some(0.75));

        // The same corpus again: the row is replaced, not duplicated.
        let mut rerun = older.clone();
        rerun.at = "2026-10-06T12:00:00Z".to_owned();
        ws.save_bench_result(&rerun);
        let rows = ws.bench_results();
        assert_eq!(rows.len(), 2, "a re-run replaces its own row");
        assert_eq!(
            rows.iter().filter(|row| row.corpus.sha256 == "aaaa").count(),
            1,
            "one row per (role, file, tier, corpus)"
        );
    }
}
