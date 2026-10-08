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

use crate::dictate::SetRecording;
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

/// One recording of the read-aloud passage, as the page lists it: what the
/// row draws and what the bench scores, one row per clip.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReadAloudRecording {
    /// The take's own directory name, which is what the page deletes by.
    pub id: String,
    pub duration_ms: u64,
    /// The wav's own byte length, which is what the recording costs on disk.
    pub bytes: u64,
    /// The wav's sha256, lowercase hex: the recording's identity, and what
    /// two recordings of the same reading share.
    pub sha256: String,
    /// RFC 3339, off the stamp the recording is named by.
    pub at: String,
}

/// The read-aloud set as the page reads it: the recordings this machine has,
/// whether one is being recorded right now, the passage they are read from,
/// the terms a run scores them on, and the last recording's failure when
/// there is one - a write that failed after the stop has no dispatch left to
/// answer, so the read carries it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReadAloudState {
    /// Oldest first, so a page that appends draws a list that grows down.
    pub recordings: Vec<ReadAloudRecording>,
    pub recording: bool,
    pub error: Option<String>,
    pub passage: String,
    /// The words of the passage a run scores term accuracy on - the figure
    /// only this corpus can produce.
    pub terms: Vec<String>,
}

/// The set directory's recordings, oldest first, read off the take
/// directories themselves: the name carries the stamp, `meta.json` the
/// length the recorder wrote, and the wav its size and its own sha. A
/// directory without its meta is one the store was interrupted writing, and
/// is skipped rather than listed as a clip the bench would then skip too.
fn read_aloud_recordings(dir: &Path) -> Vec<ReadAloudRecording> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.strip_prefix("take-").is_some_and(|rest| {
                !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit())
            })
        })
        .collect();
    names.sort();

    names
        .into_iter()
        .filter_map(|id| {
            let millis = id.strip_prefix("take-")?.parse::<i64>().ok()?;
            let take = dir.join(&id);
            let meta = std::fs::read_to_string(take.join("meta.json")).ok()?;
            let meta: serde_json::Value = serde_json::from_str(&meta).ok()?;
            let wav = take.join("output.wav");
            let at = time::OffsetDateTime::from_unix_timestamp(millis.checked_div(1000)?)
                .ok()?
                .format(&time::format_description::well_known::Rfc3339)
                .ok()?;
            Some(ReadAloudRecording {
                id,
                duration_ms: meta.get("duration_ms")?.as_u64()?,
                bytes: std::fs::metadata(&wav).ok()?.len(),
                sha256: meta.get("sha256")?.as_str()?.to_owned(),
                at,
            })
        })
        .collect()
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
    // itself scores against, so every run grew its own corpus.
    cfg.diagnostics_dir = None;
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

    /// The directory the read-aloud set lives under, beside forge's other
    /// machine-local stores; `None` when no app-support dir resolves. A
    /// test points it at its own dir rather than the real one.
    fn read_aloud_dir(&self) -> Option<std::path::PathBuf> {
        #[cfg(any(test, feature = "testing"))]
        if let Some(dir) = self.test_read_aloud_dir.lock().clone() {
            return Some(dir);
        }
        forge_sdk::app_support_dir().ok().map(|dir| dir.join("dictate-read-aloud"))
    }

    /// The read-aloud set, as the page draws it: the recordings this machine
    /// has, whether one is being recorded right now, the passage they are
    /// read from, and the last write's failure when there was one.
    pub fn read_aloud_state(&self) -> ReadAloudState {
        let dir = self.read_aloud_dir();
        let passage = forge_dictate::bench::READ_ALOUD_PASSAGE.to_owned();
        ReadAloudState {
            recordings: dir.as_deref().map(read_aloud_recordings).unwrap_or_default(),
            recording: self.dictate_runtime.lock().set_recording.is_some(),
            error: self.read_aloud_error.lock().clone(),
            terms: forge_dictate::bench::terms(&passage),
            passage,
        }
    }

    /// Drop one recording from the read-aloud set, by the id the page read.
    ///
    /// The id is a directory name this server wrote, and it is checked
    /// against that shape before anything is removed: a page-supplied path
    /// must never name a directory outside the set.
    pub(crate) fn delete_read_aloud(&self, id: &str) -> Result<(), DispatchError> {
        let Some(dir) = self.read_aloud_dir() else {
            return Err(DispatchError::ReadAloudUnavailable {
                reason: "no app-support directory resolves".to_owned(),
            });
        };
        let keep_pattern =
            |rest: &str| !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit());
        if !id.strip_prefix("take-").is_some_and(keep_pattern) {
            return Err(DispatchError::ReadAloudUnavailable {
                reason: format!("{id} is not a recording this set holds"),
            });
        }
        let path = dir.join(id);
        if let Err(error) = std::fs::remove_dir_all(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, id, "read-aloud: the recording was not deleted");
            return Err(DispatchError::ReadAloudUnavailable { reason: error.to_string() });
        }
        Ok(())
    }

    /// Begin recording the read-aloud set: the page's own microphone feeds
    /// the frames, and no engine and no transcript are involved - the
    /// passage's words are known, so recording transcribes nothing.
    ///
    /// One capture at a time, like every other: refused while a take is
    /// live, and while a recording is already running - the refusal names
    /// whatever holds it.
    pub(crate) fn start_read_aloud(&self, initiator: Option<u64>) -> Result<(), DispatchError> {
        if !self.config.dictate.enabled {
            return Err(DispatchError::DictateOff);
        }
        let mut runtime = self.dictate_runtime.lock();
        if let Some(holder) = runtime.live_holder() {
            return Err(DispatchError::TakeLive { holder });
        }
        *self.read_aloud_error.lock() = None;
        runtime.set_recording = Some(SetRecording { initiator, samples: Vec::new() });
        Ok(())
    }

    /// Feed a client's frames into the running recording, when that
    /// connection is the one that started it.
    ///
    /// The audio is capped at the same span a take holds, so a recording
    /// nobody stops cannot grow without bound; the frames past the cap are
    /// dropped rather than silently stored as a shorter reading.
    pub fn read_aloud_push(&self, samples: &[f32], initiator: Option<u64>) -> bool {
        let mut runtime = self.dictate_runtime.lock();
        let Some(recording) = runtime.set_recording.as_mut() else {
            return false;
        };
        if recording.initiator != initiator {
            return false;
        }
        let cap = usize::try_from(self.config.dictate.max_capture_minutes)
            .unwrap_or(usize::MAX)
            .saturating_mul(60)
            .saturating_mul(usize::try_from(forge_dictate::SAMPLE_RATE).unwrap_or(16_000));
        let room = cap.saturating_sub(recording.samples.len());
        let take = room.min(samples.len());
        recording.samples.extend_from_slice(&samples[..take]);
        take > 0
    }

    /// Stop the recording: `keep` writes it as the set, and a stop that does
    /// not keeps nothing. The write runs off the runtime thread, and its
    /// failure lands in the read rather than in this answer - the dispatch
    /// has already gone by the time the bytes hit the disk.
    pub(crate) fn finish_read_aloud(
        self: &Arc<Self>,
        keep: bool,
        initiator: Option<u64>,
    ) -> Result<(), DispatchError> {
        let samples = {
            let mut runtime = self.dictate_runtime.lock();
            let started_by_this_connection = runtime
                .set_recording
                .as_ref()
                .is_some_and(|recording| recording.initiator == initiator);
            if !started_by_this_connection {
                return Err(DispatchError::ReadAloudNotRecording);
            }
            let Some(recording) = runtime.set_recording.take() else {
                return Err(DispatchError::ReadAloudNotRecording);
            };
            recording.samples
        };
        if !keep {
            return Ok(());
        }
        let Some(dir) = self.read_aloud_dir() else {
            *self.read_aloud_error.lock() = Some("no app-support directory resolves".to_owned());
            return Ok(());
        };
        if samples.is_empty() {
            *self.read_aloud_error.lock() = Some("nothing was captured".to_owned());
            return Ok(());
        }

        let this = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let outcome = forge_dictate::bench::store_read_aloud(&dir, &samples);
            match outcome {
                Ok(()) => {}
                Err(error) => {
                    tracing::warn!(
                        event_name = "read_aloud_write_failed",
                        %error,
                        dir = %dir.display(),
                        "the read-aloud set was not written"
                    );
                    *this.read_aloud_error.lock() = Some(error.to_string());
                }
            }
            this.push_models();
        });
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
    /// [`SessionUpdate::DictateModelsChanged`](crate::SessionUpdate::DictateModelsChanged),
    /// the way every other dictate action's does.
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

    /// Drop one saved result: the row the page asked to delete, by its key.
    pub(crate) fn delete_bench_result(
        &self,
        target: &BenchTarget,
        tier: forge_dictate::bench::Tier,
        corpus: &str,
    ) -> Result<(), DispatchError> {
        let db = self.db.lock();
        if let Some(db) = db.as_ref() {
            let key =
                crate::store::bench_results::key_parts(target.role, &target.file, tier, corpus);
            if let Err(error) = crate::store::bench_results::remove(db, &key) {
                tracing::warn!(
                    event_name = "dictate_bench_delete_failed",
                    %error,
                    "the saved result was not deleted"
                );
            }
        }
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
        let read_aloud_dir = self.read_aloud_dir();
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
                return Err(match tier {
                    forge_dictate::bench::Tier::Consensus => {
                        "nothing to bench: no takes have been saved here yet - dictate a take, \
                         or record the read-aloud passage and score that"
                            .to_owned()
                    }
                    forge_dictate::bench::Tier::ReadAloud => {
                        "nothing to bench: the read-aloud passage has not been recorded yet"
                            .to_owned()
                    }
                });
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
            for_it.diagnostics_dir.is_none(),
            "a bench run must not write takes into the store it scores against"
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

    /// The last models frame queued so far, draining what came before it.
    fn drained_models(
        updates: &mut tokio::sync::mpsc::UnboundedReceiver<crate::SessionUpdate>,
    ) -> Option<crate::catalogue::DictateModelsSnapshot> {
        let mut found = None;
        while let Ok(update) = updates.try_recv() {
            if let crate::SessionUpdate::DictateModelsChanged { models } = update {
                found = Some(models);
            }
        }
        found
    }

    /// **Starting and stopping a recording both push the read.** The card is
    /// drawn from a frame like every other fact on the page, so a command
    /// that moves the recording without a frame leaves the press looking
    /// dead.
    #[test]
    fn starting_and_stopping_a_recording_push_the_models_read() {
        let (ws, mut updates, _models) = crate::catalogue::tests_catalogue_view::enabled_stub();
        let dir = tempfile::tempdir().unwrap();
        *ws.test_read_aloud_dir.lock() = Some(dir.path().to_path_buf());

        ws.dispatch(crate::Command::DictateReadAloudStart { initiator: Some(1) })
            .expect("nothing else is capturing");
        let models = drained_models(&mut updates).expect("starting pushes the read");
        assert!(models.read_aloud.recording, "the frame carries the recording");

        ws.dispatch(crate::Command::DictateReadAloudStop { keep: false, initiator: Some(1) })
            .expect("the recording is this connection's");
        let models = drained_models(&mut updates).expect("stopping pushes the read");
        assert!(!models.read_aloud.recording, "the frame carries it stopped");
    }

    /// The next frame that carries a written set, inside a test's patience:
    /// the write runs off the runtime thread, so the stop's own frame lands
    /// before it and this is the one that says the bytes are on disk.
    async fn await_recorded(
        updates: &mut tokio::sync::mpsc::UnboundedReceiver<crate::SessionUpdate>,
    ) -> crate::catalogue::DictateModelsSnapshot {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                match updates.recv().await {
                    Some(crate::SessionUpdate::DictateModelsChanged { models }) => {
                        if !models.read_aloud.recordings.is_empty() {
                            break models;
                        }
                    }
                    Some(_) => {}
                    None => panic!("the subscription must stay attached"),
                }
            }
        })
        .await
        .expect("the set must land inside fifteen seconds")
    }

    /// **A kept recording becomes the set.** The frames the page fed land as
    /// the passage's own take - and only the recording connection's frames
    /// do: another connection's are not the recording's to feed. A stop that
    /// does not keep writes nothing.
    #[tokio::test]
    async fn a_kept_recording_becomes_the_set_and_a_dropped_one_writes_nothing() {
        let (ws, mut updates, _models) = crate::catalogue::tests_catalogue_view::enabled_stub();
        let dir = tempfile::tempdir().unwrap();
        *ws.test_read_aloud_dir.lock() = Some(dir.path().to_path_buf());

        ws.dispatch(crate::Command::DictateReadAloudStart { initiator: Some(1) }).unwrap();
        assert!(ws.read_aloud_push(&vec![0.25_f32; 1_600], Some(1)), "its own frames land");
        assert!(!ws.read_aloud_push(&[0.0_f32; 8], Some(2)), "another connection's do not");
        ws.dispatch(crate::Command::DictateReadAloudStop { keep: true, initiator: Some(1) })
            .unwrap();

        let models = await_recorded(&mut updates).await;
        assert_eq!(models.read_aloud.recordings.len(), 1, "the read says the set is here");
        let clips = forge_dictate::bench::read_aloud(dir.path()).expect("the set reads");
        assert_eq!(clips.len(), 1, "a passage and a take are there");
        assert_eq!(clips[0].audio.len(), 1_600, "the frames the page fed");

        // A second recording, dropped rather than kept: nothing replaces the
        // set that stands.
        ws.dispatch(crate::Command::DictateReadAloudStart { initiator: Some(1) }).unwrap();
        assert!(ws.read_aloud_push(&vec![0.5_f32; 800], Some(1)));
        ws.dispatch(crate::Command::DictateReadAloudStop { keep: false, initiator: Some(1) })
            .unwrap();
        let clips = forge_dictate::bench::read_aloud(dir.path()).unwrap();
        assert_eq!(clips.len(), 1, "a dropped recording leaves the set alone");
        assert_eq!(clips[0].audio.len(), 1_600);
    }

    /// **A recording is deleted by the id the page read, and nothing else
    /// is.** The id is a directory name this server wrote, and a page-supplied
    /// path must never name a directory outside the set.
    #[tokio::test]
    async fn a_recording_is_deleted_by_its_id_and_a_foreign_one_is_refused() {
        let (ws, mut updates, _models) = crate::catalogue::tests_catalogue_view::enabled_stub();
        let dir = tempfile::tempdir().unwrap();
        *ws.test_read_aloud_dir.lock() = Some(dir.path().to_path_buf());

        ws.dispatch(crate::Command::DictateReadAloudStart { initiator: Some(1) }).unwrap();
        assert!(ws.read_aloud_push(&vec![0.25_f32; 1_600], Some(1)));
        ws.dispatch(crate::Command::DictateReadAloudStop { keep: true, initiator: Some(1) })
            .unwrap();
        let models = await_recorded(&mut updates).await;
        let id = models.read_aloud.recordings[0].id.clone();
        assert_eq!(models.read_aloud.recordings[0].duration_ms, 100, "a tenth of a second");

        // A path that is not one of this set's own directory names is refused
        // rather than removed.
        let outside = dir.path().join("neighbour");
        std::fs::create_dir_all(&outside).unwrap();
        for id in ["../neighbour", "neighbour", "take-x", "take-"] {
            let err = ws.dispatch(crate::Command::DictateReadAloudDelete { id: id.to_owned() });
            assert!(err.is_err(), "accepted {id:?}");
        }
        assert!(outside.is_dir(), "a foreign directory must survive");

        ws.dispatch(crate::Command::DictateReadAloudDelete { id: id.clone() }).unwrap();
        let models = drained_models(&mut updates).expect("the delete pushes the read");
        assert!(models.read_aloud.recordings.is_empty(), "the recording left the set");
        assert!(!dir.path().join(&id).exists(), "and its directory went with it");
    }

    /// One capture at a time: a take that is live refuses a recording by
    /// name, and a recording counts as live for the guards that wait on
    /// captures.
    #[test]
    fn a_recording_and_a_take_refuse_each_other() {
        let (ws, _updates, _models) = crate::catalogue::tests_catalogue_view::enabled_stub();
        let dir = tempfile::tempdir().unwrap();
        *ws.test_read_aloud_dir.lock() = Some(dir.path().to_path_buf());

        ws.dispatch(crate::Command::DictateReadAloudStart { initiator: Some(1) }).unwrap();
        assert_eq!(
            ws.dictate_runtime.lock().live_holder().as_deref(),
            Some("the read-aloud recording"),
            "a recording is a live capture"
        );
        let err = ws
            .dispatch(crate::Command::DictateReadAloudStart { initiator: Some(1) })
            .expect_err("one recording at a time");
        assert!(
            matches!(err, crate::DispatchError::TakeLive { ref holder } if holder == "the read-aloud recording"),
            "got: {err:?}"
        );

        let (stop, _stop_rx) = tokio::sync::mpsc::channel(1);
        ws.dictate_runtime.lock().recordings.insert(
            crate::SessionSlot::new("Busytools", "forge", "worker"),
            crate::dictate::LiveRecording { stop, sink: None, initiator: None },
        );
        ws.dispatch(crate::Command::DictateReadAloudStop { keep: false, initiator: Some(1) })
            .unwrap();
        let err = ws
            .dispatch(crate::Command::DictateReadAloudStart { initiator: Some(1) })
            .expect_err("a take holds the capture");
        assert!(
            matches!(&err, crate::DispatchError::TakeLive { holder } if holder == "Busytools/forge/worker"),
            "the refusal names the seat whose take is live, got: {err:?}"
        );
    }
}
