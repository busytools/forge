//! The bench's corpus: the audio a run scores against, taken from this
//! machine's own material.
//!
//! **Two sources, and what each can say is different.** A saved take is
//! this machine's own speech with the in-use model's words beside it -
//! latency on real material, but its `text.txt` is a model's output, not
//! truth: the consensus tier reads how far the candidate sits from the
//! model in use, which is a signal and not an error. The read-aloud set is
//! the one source whose words are known, because they are a passage
//! somebody read aloud on purpose - only a clip from it can be scored for
//! term accuracy, and everything here keeps that distinction in the types.
//!
//! **Nothing is embedded.** A shipped binary carries no audio; a machine
//! with no material says so, and the way to get material is to record it.

use std::path::Path;
use std::time::Duration;

use sha2::{Digest as _, Sha256};

use crate::{Config, Error, SAMPLE_RATE, Stages};

/// Which clips a run scores against, in the names the page uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// The saved takes: latency on this machine's own dictation, and how
    /// far the candidate's words sit from the in-use model's on the same
    /// clips.
    Consensus,
    /// The read-aloud set alone: the one tier whose words are known.
    ReadAloud,
}

/// Where one clip came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipSource {
    Take,
    ReadAloud,
}

/// One clip of audio, and whatever is known about it.
pub struct Clip {
    pub source: ClipSource,
    pub audio: Vec<f32>,
    /// The words a run is scored against where they are KNOWN: the
    /// read-aloud passage, and nothing else. `None` for a saved take,
    /// whose `text.txt` is a model's output rather than truth.
    pub truth: Option<String>,
    /// The in-use model's own words for this audio, where a take saved
    /// them.
    pub recorded: Option<String>,
}

/// What two runs are comparable by: the corpus's own shape and bytes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CorpusId {
    pub clips: usize,
    pub audio_seconds: u64,
    pub sha256: String,
}

/// The audio a run scores against, and its identity.
pub struct Corpus {
    pub clips: Vec<Clip>,
    pub id: CorpusId,
}

/// Build one tier's corpus from the machine's own material.
pub fn corpus(tier: Tier, takes_dir: &Path, read_aloud_dir: &Path) -> Result<Corpus, Error> {
    let clips = match tier {
        Tier::Consensus => takes(takes_dir)?,
        Tier::ReadAloud => read_aloud(read_aloud_dir)?.into_iter().collect(),
    };
    let id = corpus_id(&clips);
    Ok(Corpus { clips, id })
}

/// Every complete take in `dir`, oldest first.
///
/// A take whose `meta.json` is absent is one the store was still writing -
/// that file lands last, which is what makes a directory complete - and it
/// is skipped rather than half-read.
pub fn takes(dir: &Path) -> Result<Vec<Clip>, Error> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        // A machine that has never recorded has no store, and that is an
        // empty corpus rather than a failure: the run says so with its
        // own zero.
        return Ok(Vec::new());
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
    // The stamp is zero-padded to 13 digits, so the names sort by recency.
    names.sort();

    let mut clips = Vec::new();
    for name in names {
        let take = dir.join(&name);
        if !take.join("meta.json").is_file() {
            continue;
        }
        let wav = take.join("output.wav");
        if !wav.is_file() {
            continue;
        }
        let audio = decode_wav(&wav)?;
        let recorded = std::fs::read_to_string(take.join("text.txt")).ok();
        clips.push(Clip { source: ClipSource::Take, audio, truth: None, recorded });
    }
    Ok(clips)
}

/// The read-aloud set, when this machine has recorded one.
///
/// The set is a passage beside the take that read it: `passage.txt` is the
/// truth, and the newest take under the same directory is the audio - kept
/// whole, so a re-run scores the same bytes.
pub fn read_aloud(dir: &Path) -> Result<Option<Clip>, Error> {
    let passage = dir.join("passage.txt");
    if !passage.is_file() {
        return Ok(None);
    }
    let truth = std::fs::read_to_string(&passage)
        .map_err(|source| Error::Io { path: passage.clone(), source })?;

    let framed: Vec<Clip> = takes(dir)?;
    let Some(clip) = framed.into_iter().last() else {
        // A passage with no take under it is a recording that never finished;
        // the caller sees no set rather than a silent clip.
        return Ok(None);
    };
    Ok(Some(Clip { source: ClipSource::ReadAloud, truth: Some(truth), ..clip }))
}

/// Write one recording as the read-aloud set: the passage read, the take the
/// words were recorded in, and a manifest naming the wav's own sha256.
///
/// The set REPLACES whatever stood there - the previous take goes first, so
/// a half-written replacement never leaves two takes under one passage - and
/// the take's `meta.json` lands last, which is what makes it complete to
/// [`read_aloud`]: an interrupted write reads as no set rather than as a
/// partial one. The outcome is the caller's to report, because the recording
/// is a press somebody is watching.
pub fn store_read_aloud(dir: &Path, samples: &[f32]) -> Result<(), Error> {
    std::fs::create_dir_all(dir).map_err(|source| Error::Io { path: dir.to_path_buf(), source })?;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("take-") {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }

    let take = format!("take-{:013}", crate::diagnostics::take_stamp());
    let take_dir = dir.join(&take);
    std::fs::create_dir_all(&take_dir)
        .map_err(|source| Error::Io { path: take_dir.clone(), source })?;
    let wav = take_dir.join("output.wav");
    crate::diagnostics::write_wav(&wav, samples)
        .map_err(|message| Error::Bench { message: format!("{}: {message}", wav.display()) })?;
    let meta = serde_json::json!({
        "duration_ms": u64::try_from(samples.len()).unwrap_or(u64::MAX) / 16,
    });
    let bytes = serde_json::to_vec_pretty(&meta)
        .map_err(|error| Error::Bench { message: format!("the set's meta: {error}") })?;
    let meta_path = take_dir.join("meta.json");
    std::fs::write(&meta_path, bytes).map_err(|source| Error::Io { path: meta_path, source })?;

    let passage = dir.join("passage.txt");
    std::fs::write(&passage, READ_ALOUD_PASSAGE)
        .map_err(|source| Error::Io { path: passage, source })?;
    let manifest = serde_json::json!({
        "sha256": crate::diagnostics::sha256_file(&wav),
        "take": take,
    });
    let bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| Error::Bench { message: format!("the set's manifest: {error}") })?;
    let manifest_path = dir.join("manifest.json");
    std::fs::write(&manifest_path, bytes)
        .map_err(|source| Error::Io { path: manifest_path, source })
}

/// The identity two runs are comparable by: the clip count, the seconds of
/// audio, and a hash over every clip's own samples, so two corpora that
/// differ by one take cannot share an id.
fn corpus_id(clips: &[Clip]) -> CorpusId {
    let mut hasher = Sha256::new();
    for clip in clips {
        hasher.update((clip.audio.len() as u64).to_le_bytes());
        for sample in &clip.audio {
            hasher.update(sample.to_le_bytes());
        }
    }
    let samples: u64 = clips.iter().map(|clip| clip.audio.len() as u64).sum();
    CorpusId {
        clips: clips.len(),
        audio_seconds: samples / u64::from(SAMPLE_RATE),
        sha256: hex::encode(hasher.finalize()),
    }
}

/// The words a mishearing shows up in, across the whole corpus. A term
/// survives when it comes out whole and spelled right, case-insensitively -
/// which is the figure a bench exists to move.
const KNOWN_TERMS: [&str; 11] = [
    "playwright",
    "tauri",
    "forge",
    "redb",
    "mcp",
    "gguf",
    "fleurs",
    "parakeet",
    "cohere",
    "granite",
    "whisper",
];

/// The passage the read-aloud set is recorded from: the one document whose
/// words are known, so a run over it can be scored rather than compared.
/// Read aloud once, kept like a model artifact, and re-used for every
/// future candidate.
///
/// It carries several of the known terms - the words a dictation model
/// actually gets wrong - so the figure it yields moves for the reason it
/// exists.
pub const READ_ALOUD_PASSAGE: &str = "\
I want the forge session to pick up where it left off, so open the rail and check which agents are still running. The Tauri client talks to the socket on port 8790, and the transcript is folded into turns before the page draws it. Push the Playwright suite after the redb migration lands, then run the gate once more against main. The MCP server hands the tool list to the session at startup, and the GGUF weights are checked by size before any load. If the gateway cannot bind, the accounts card says so and the spawn is refused.";

/// The passage's own terms: every word in it the vocabulary knows, whole
/// words only, deduplicated, in the passage's order.
pub fn terms(passage: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for word in words(passage) {
        if KNOWN_TERMS.contains(&word.as_str()) && !found.contains(&word) {
            found.push(word);
        }
    }
    found
}

/// Term accuracy and word error rate of `text` against `truth`.
///
/// **Term accuracy is the share of `terms` that survived** as whole words -
/// `playwright` misspelled as `playright` is a miss, in the middle of an
/// otherwise perfect sentence. WER is Levenshtein edits over words over
/// the truth's word count; a truth with no words reads as no error rather
/// than dividing by zero. Neither figure is meaningful for a take, whose
/// truth is a model's own output; only the read-aloud passage carries
/// words somebody knows were said.
pub fn score(text: &str, truth: &str, terms: &[String]) -> (f64, f64) {
    let survived = terms.iter().filter(|term| words(text).contains(term)).count();
    let truth_words = words(truth);
    let accuracy = ratio(survived, terms.len());
    let wer = ratio(edits(&words(text), &truth_words), truth_words.len());
    (accuracy, wer)
}

/// A string's words: lowercased, split on anything that is not a letter or
/// a digit, blanks dropped. The one spelling both axes compare on.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// One count's share of another, `0.0` when there is nothing to divide by.
///
/// The conversions go through `u32` because [`f64::from`] only takes
/// those: every count this module forms is words, terms or clips, and
/// four billion of any is far past what a corpus or a sentence holds, so
/// the saturation arm is unreachable in fact and only keeps the function
/// total.
pub fn ratio(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    f64::from(u32::try_from(part).unwrap_or(u32::MAX))
        / f64::from(u32::try_from(whole).unwrap_or(u32::MAX))
}

/// A count of samples as seconds at the model's own rate.
fn samples_seconds(samples: usize) -> f64 {
    f64::from(u32::try_from(samples).unwrap_or(u32::MAX)) / f64::from(SAMPLE_RATE)
}

/// Word-level Levenshtein distance: the edits between two word sequences.
fn edits(text: &[String], truth: &[String]) -> usize {
    let mut previous: Vec<usize> = (0..=truth.len()).collect();
    let mut current = vec![0_usize; truth.len() + 1];
    for (row, word) in text.iter().enumerate() {
        current[0] = row + 1;
        for (column, against) in truth.iter().enumerate() {
            let substitute = previous[column] + usize::from(word != against);
            current[column + 1] = substitute.min(previous[column + 1] + 1).min(current[column] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[truth.len()]
}

/// One clip's outcome, as the callback sees it while a run is out.
pub struct ClipRun {
    pub index: usize,
    pub text: String,
    pub stages: Stages,
    pub wall: Duration,
    /// Whether the run's words are the same as what this clip already
    /// carries - another model's output, which is the in-use model's own
    /// text for a saved take. `None` on a read-aloud clip, whose truth is
    /// a passage rather than a model, and on a take that saved none.
    pub matched: Option<bool>,
}

/// Where one run's time went, summed over its clips.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StageTotals {
    pub model_load_ms: u64,
    pub resample_ms: u64,
    pub mel_ms: u64,
    pub encode_ms: u64,
    pub decode_ms: u64,
    pub normalize_ms: u64,
}

/// What one run measured.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Metrics {
    pub clips: usize,
    pub audio_seconds: f64,
    pub wall_seconds: f64,
    /// The headline speed: audio seconds per wall second, the feed's own
    /// axis.
    pub xrt_wall: f64,
    /// The share of the passage's known terms that survived. `None`
    /// without a read-aloud clip, because nothing else has words anybody
    /// knows were said.
    pub term_accuracy: Option<f64>,
    /// Mean word error rate against every clip that carries a truth,
    /// which only the read-aloud passage does: `None` on a tier without
    /// one.
    pub wer: Option<f64>,
    /// How many clips the run agreed with word-for-word, of the clips
    /// that had something to compare against. The consensus signal;
    /// `None` when no clip did.
    pub matched: Option<(usize, usize)>,
    pub stages_ms: StageTotals,
}

/// One clip's transcription as the loop needs it.
struct ClipOutcome {
    text: String,
    stages: Stages,
}

/// Walk the corpus, handing each clip's outcome to `on_clip`; a `Break`
/// stops the run where it stands.
fn walk(
    corpus: &Corpus,
    mut transcribe: impl FnMut(usize, &Clip) -> Result<ClipOutcome, Error>,
    mut on_clip: impl FnMut(&ClipRun) -> std::ops::ControlFlow<()>,
) -> Result<Vec<ClipRun>, Error> {
    let mut runs = Vec::new();
    for (index, clip) in corpus.clips.iter().enumerate() {
        let started = std::time::Instant::now();
        let outcome = transcribe(index, clip)?;
        let wall = started.elapsed();
        let against = match clip.source {
            ClipSource::ReadAloud => None,
            ClipSource::Take => clip.recorded.as_ref(),
        };
        let matched = against.map(|other| words(&outcome.text) == words(other));
        let run = ClipRun { index, text: outcome.text, stages: outcome.stages, wall, matched };
        let stopped = on_clip(&run) == std::ops::ControlFlow::Break(());
        runs.push(run);
        if stopped {
            return Err(Error::Cancelled);
        }
    }
    Ok(runs)
}

/// Run one corpus through one config, reporting each clip as it lands.
///
/// The engine is built and made ready first, so a model that will not load
/// fails here rather than on the first clip. A `Break` from `on_clip`
/// returns [`Error::Cancelled`]: what ran is not a result, and the caller
/// saves nothing.
pub fn run(
    cfg: &Config,
    corpus: &Corpus,
    mut on_clip: impl FnMut(&ClipRun) -> std::ops::ControlFlow<()>,
) -> Result<Metrics, Error> {
    let engine = crate::Engine::new(cfg.clone())?;
    engine.wait_ready()?;

    let runs = walk(
        corpus,
        |index, clip| {
            let outcome = engine.transcribe(crate::Samples::mono(clip.audio.clone()))?.recv()?;
            match outcome {
                crate::Outcome::Transcript(transcript) => {
                    Ok(ClipOutcome { text: transcript.text, stages: transcript.stages })
                }
                // A clip with no audio is the corpus's problem rather than
                // recognition's: the material is supposed to be speech.
                other @ crate::Outcome::NoAudio { .. } => Err(Error::Bench {
                    message: format!("clip {index} produced no transcript: {other:?}"),
                }),
            }
        },
        &mut on_clip,
    )?;
    if runs.is_empty() {
        return Err(Error::Bench { message: "the corpus has no clips to run".to_owned() });
    }

    // The passage is the only truth whose words are known; every other
    // clip's truth is another model's output.
    let term_accuracy =
        corpus.clips.iter().position(|clip| clip.source == ClipSource::ReadAloud).and_then(
            |index| {
                let truth = corpus.clips.get(index)?.truth.clone()?;
                let passage_terms = terms(&truth);
                runs.iter()
                    .find(|run| run.index == index)
                    .map(|run| score(&run.text, &truth, &passage_terms).0)
            },
        );

    let audio_samples: usize = corpus.clips.iter().map(|clip| clip.audio.len()).sum();
    let audio_seconds = samples_seconds(audio_samples);
    let wall_seconds: f64 = runs.iter().map(|run| run.wall.as_secs_f64()).sum();

    let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    let mut stages_ms = StageTotals::default();
    for run in &runs {
        stages_ms.model_load_ms += ms(run.stages.model_load);
        stages_ms.resample_ms += ms(run.stages.resample);
        stages_ms.mel_ms += ms(run.stages.mel);
        stages_ms.encode_ms += ms(run.stages.encode);
        stages_ms.decode_ms += ms(run.stages.decode);
        stages_ms.normalize_ms += run.stages.normalize.map_or(0, ms);
    }

    let mut edits_total = 0_usize;
    let mut truth_total = 0_usize;
    for run in &runs {
        let Some(clip) = corpus.clips.get(run.index) else { continue };
        let Some(truth) = clip.truth.as_deref() else { continue };
        let truth_words = words(truth);
        if truth_words.is_empty() {
            continue;
        }
        edits_total += edits(&words(&run.text), &truth_words);
        truth_total += truth_words.len();
    }
    let wer = (truth_total > 0).then(|| ratio(edits_total, truth_total));

    let scored: Vec<&ClipRun> = runs.iter().filter(|run| run.matched.is_some()).collect();
    let matched = (!scored.is_empty()).then(|| {
        let agreed = scored.iter().filter(|run| run.matched == Some(true)).count();
        (agreed, scored.len())
    });

    Ok(Metrics {
        clips: runs.len(),
        audio_seconds,
        wall_seconds,
        xrt_wall: if wall_seconds > 0.0 { audio_seconds / wall_seconds } else { 0.0 },
        term_accuracy,
        wer,
        matched,
        stages_ms,
    })
}

/// Decode one 16-bit mono wav at the model's own rate, refusing anything
/// else by name rather than resampling it.
fn decode_wav(path: &Path) -> Result<Vec<f32>, Error> {
    let reader = hound::WavReader::open(path)
        .map_err(|error| Error::Bench { message: format!("{}: {error}", path.display()) })?;
    decode_reader(reader, &path.display().to_string())
}

fn decode_reader<R: std::io::Read>(
    mut reader: hound::WavReader<R>,
    name: &str,
) -> Result<Vec<f32>, Error> {
    let spec = reader.spec();
    if spec.sample_rate != SAMPLE_RATE {
        return Err(Error::SampleRate { expected: SAMPLE_RATE, actual: spec.sample_rate });
    }
    if spec.channels != 1 {
        return Err(Error::Channels { actual: spec.channels });
    }
    let samples: Result<Vec<f32>, _> = reader
        .samples::<i16>()
        .map(|sample| sample.map(|value| f32::from(value) / 32768.0))
        .collect();
    samples.map_err(|error| Error::Bench { message: format!("{name}: {error}") })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One complete take, the way the store writes one: the wav, the text,
    /// and the meta that lands last.
    fn write_take(dir: &Path, stamp: u128, samples: usize, text: &str) {
        let take = dir.join(format!("take-{stamp:013}"));
        std::fs::create_dir_all(&take).unwrap();
        write_wav(&take.join("output.wav"), samples);
        std::fs::write(take.join("text.txt"), text).unwrap();
        std::fs::write(take.join("meta.json"), "{}").unwrap();
    }

    fn write_wav(path: &Path, samples: usize) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for n in 0..samples {
            writer.write_sample(i16::try_from(n % 100).unwrap()).unwrap();
        }
        writer.finalize().unwrap();
    }

    /// A take is complete when its meta landed; anything else is a store
    /// mid-write and is skipped rather than half-read. Order is the
    /// stamp's, oldest first.
    #[test]
    fn reads_every_complete_take_oldest_first_and_skips_incomplete_ones() {
        let dir = tempfile::tempdir().unwrap();
        write_take(dir.path(), 2, 4, "second");
        write_take(dir.path(), 1, 4, "first");
        // Incomplete: the wav and text are there, the meta is not.
        let half = dir.path().join("take-0000000000003");
        std::fs::create_dir_all(&half).unwrap();
        write_wav(&half.join("output.wav"), 4);
        std::fs::write(half.join("text.txt"), "half").unwrap();
        // Foreign: a directory sharing the prefix that is not a take.
        std::fs::create_dir_all(dir.path().join("take-2026-photos")).unwrap();

        let clips = takes(dir.path()).unwrap();

        assert_eq!(clips.len(), 2, "one clip per complete take");
        assert_eq!(clips[0].recorded.as_deref(), Some("first"), "oldest first");
        assert_eq!(clips[1].recorded.as_deref(), Some("second"));
        assert_eq!(clips[0].audio.len(), 4, "the wav decodes whole");
        assert!(clips.iter().all(|clip| clip.source == ClipSource::Take));
        assert!(
            clips.iter().all(|clip| clip.truth.is_none()),
            "a take's own text is a model's output, not truth"
        );
    }

    /// A machine that never recorded has no store, and that is an empty
    /// corpus rather than a failure.
    #[test]
    fn a_store_that_is_not_there_is_empty_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let none = dir.path().join("never-created");

        assert!(takes(&none).unwrap().is_empty());
    }

    /// The corpus's identity moves with its clips: the count, the seconds,
    /// and a hash over the samples, so two takes of different audio cannot
    /// share one.
    #[test]
    fn a_corpus_id_follows_its_clips() {
        let dir = tempfile::tempdir().unwrap();
        write_take(dir.path(), 1, 16_000, "one");
        let empty = tempfile::tempdir().unwrap();

        let one = corpus(Tier::Consensus, dir.path(), empty.path()).unwrap();
        let again = corpus(Tier::Consensus, dir.path(), empty.path()).unwrap();
        assert_eq!(one.id, again.id, "the same clips are the same corpus");
        assert_eq!(one.id.clips, 1);
        assert_eq!(one.id.audio_seconds, 1, "16k samples is one second");

        write_take(dir.path(), 2, 8_000, "two");
        let two = corpus(Tier::Consensus, dir.path(), empty.path()).unwrap();
        assert_ne!(one.id.sha256, two.id.sha256, "one more take is a different corpus");
        assert_eq!(two.id.clips, 2, "the tier is the machine's own takes and nothing else");

        let gold = corpus(Tier::ReadAloud, empty.path(), empty.path()).unwrap();
        assert!(gold.clips.is_empty(), "no passage recorded, no read-aloud tier");
    }

    /// **A recording becomes the set, and a later one replaces it.** The
    /// take carries the wav and its meta - what the reader needs to see it
    /// as complete - the manifest names the wav's own sha, and the previous
    /// take leaves with the previous recording rather than stacking under
    /// one passage.
    #[test]
    fn a_recording_becomes_the_set_and_a_later_one_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        let audio = vec![0.25_f32; 1_600];

        store_read_aloud(dir.path(), &audio).expect("the set writes");
        let clip =
            read_aloud(dir.path()).expect("the set reads").expect("a passage and a take are there");
        assert_eq!(clip.source, ClipSource::ReadAloud);
        assert_eq!(clip.truth.as_deref(), Some(READ_ALOUD_PASSAGE));
        assert_eq!(clip.audio.len(), audio.len(), "the samples the recording carried");
        let first = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .find(|name| name.starts_with("take-"))
            .expect("the take is beside the passage");
        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert!(
            manifest["sha256"].as_str().is_some_and(|sha| sha.len() == 64),
            "the manifest names the wav's own sha, got {manifest}"
        );

        store_read_aloud(dir.path(), &audio[..800]).expect("a re-recording writes");
        let takes: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("take-"))
            .collect();
        assert_eq!(takes.len(), 1, "a re-recording replaces the set, got {takes:?}");
        assert!(!dir.path().join(&first).exists(), "the previous take left with it");
        assert_eq!(
            read_aloud(dir.path()).unwrap().unwrap().audio.len(),
            800,
            "the set reads back as the newest recording"
        );
    }

    /// **Term accuracy counts the passage's terms that survived.** A miss
    /// lands in the middle of an otherwise perfect sentence - the shape a
    /// misheard proper noun actually has - and moves the figure; nothing
    /// else does.
    /// The passage carries terms, and enough of them: a gold set whose
    /// text has nothing a model can get wrong would measure nothing.
    #[test]
    fn the_read_aloud_passage_carries_terms_worth_scoring() {
        let found = terms(READ_ALOUD_PASSAGE);

        assert!(found.len() >= 5, "the passage must carry several known terms, found: {found:?}");
        assert!(found.contains(&"playwright".to_owned()));
        assert!(found.contains(&"gguf".to_owned()));
    }

    #[test]
    fn term_accuracy_counts_the_passages_terms_that_survived() {
        let truth = "Push the Playwright suite and then check redb.";
        let found = terms(truth);
        assert_eq!(found.len(), 2, "the passage names two known terms: {found:?}");

        let (accuracy, _) = score("Push the Playright suite and then check redb.", truth, &found);
        assert!((accuracy - 0.5).abs() < f64::EPSILON, "one of two survived, got {accuracy}");

        let (perfect, _) = score(truth, truth, &found);
        assert!((perfect - 1.0).abs() < f64::EPSILON);
    }

    /// WER is edits over the truth's words, by word.
    #[test]
    fn wer_is_edits_over_the_passages_words() {
        let (_, wer) = score("one two three", "one two four three", &[]);

        assert!((wer - 0.25).abs() < 1e-9, "one insertion in four words, got {wer}");
    }

    /// A `Break` from the callback stops the walk where it stands, and the
    /// caller hears a cancellation rather than a truncated result.
    #[test]
    fn a_break_from_the_callback_stops_the_loop_early() {
        let dir = tempfile::tempdir().unwrap();
        write_take(dir.path(), 1, 8, "one");
        write_take(dir.path(), 2, 8, "two");
        write_take(dir.path(), 3, 8, "three");
        let empty = tempfile::tempdir().unwrap();
        let built = corpus(Tier::Consensus, dir.path(), empty.path()).unwrap();

        let mut called = 0;
        let mut transcribed = 0;
        let outcome = walk(
            &built,
            |_index, _clip| {
                transcribed += 1;
                Ok(ClipOutcome { text: "word".to_owned(), stages: Stages::default() })
            },
            |run| {
                called += 1;
                if run.index == 1 {
                    std::ops::ControlFlow::Break(())
                } else {
                    std::ops::ControlFlow::Continue(())
                }
            },
        );

        assert!(matches!(outcome, Err(Error::Cancelled)), "a stop is a cancellation");
        assert_eq!(called, 2, "the callback saw the clip that broke and no later one");
        assert_eq!(transcribed, 2, "and nothing was transcribed past the break");
    }

    /// The agreement axis: a take is compared against the in-use model's
    /// own recorded words, word-for-word once case and punctuation are
    /// out of the way; a read-aloud clip carries no comparison at all.
    #[test]
    fn a_clip_is_matched_against_the_other_models_words_not_the_passage() {
        let dir = tempfile::tempdir().unwrap();
        write_take(dir.path(), 1, 8, "Hello, world!");
        write_take(dir.path(), 2, 8, "Something else entirely.");
        let empty = tempfile::tempdir().unwrap();
        let built = corpus(Tier::Consensus, dir.path(), empty.path()).unwrap();

        let runs = walk(
            &built,
            |_index, clip| {
                let text = if clip.recorded.as_deref() == Some("Hello, world!") {
                    "hello world".to_owned()
                } else {
                    "different words".to_owned()
                };
                Ok(ClipOutcome { text, stages: Stages::default() })
            },
            |_| std::ops::ControlFlow::Continue(()),
        )
        .unwrap();

        assert_eq!(runs[0].matched, Some(true), "case and punctuation are out of the way");
        assert_eq!(runs[1].matched, Some(false));

        let passage = Clip {
            source: ClipSource::ReadAloud,
            audio: vec![0.0; 8],
            truth: Some("a passage".to_owned()),
            recorded: None,
        };
        let gold = Corpus { id: corpus_id(std::slice::from_ref(&passage)), clips: vec![passage] };
        let runs = walk(
            &gold,
            |_index, _clip| {
                Ok(ClipOutcome { text: "a passage".to_owned(), stages: Stages::default() })
            },
            |_| std::ops::ControlFlow::Continue(()),
        )
        .unwrap();

        assert_eq!(runs[0].matched, None, "a passage is scored by term accuracy, not by agreement");
    }
}
