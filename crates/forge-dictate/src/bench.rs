//! The bench's corpus: the audio a run scores against, taken from this
//! machine's own material.
//!
//! **Three sources, and what each can say is different.** A saved take is
//! this machine's own speech with the in-use model's words beside it -
//! latency on real material, but its `text.txt` is a model's output, not
//! truth. A fixture's baseline came from another model too, so its text is
//! an output as well: the consensus tier reads how far two models sit from
//! each other, which is a signal and not an error. The read-aloud set is
//! the one source whose words are known, because they are a passage
//! somebody read aloud on purpose - only a clip from it can be scored for
//! term accuracy, and everything here keeps that distinction in the types.

use std::io::Cursor;
use std::path::Path;

use sha2::{Digest as _, Sha256};

use crate::{Error, SAMPLE_RATE};

/// Which clips a run scores against, in the names the page uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// The saved takes alone: latency on this machine's own dictation.
    Latency,
    /// The saved takes plus the repo fixtures: how far the candidate's
    /// words sit from the baselines, which are another model's.
    Consensus,
    /// The read-aloud set alone: the one tier whose words are known.
    ReadAloud,
}

/// Where one clip came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipSource {
    Take,
    Fixture,
    ReadAloud,
}

/// One clip of audio, and whatever is known about it.
pub struct Clip {
    pub source: ClipSource,
    pub audio: Vec<f32>,
    /// Words a run is scored against where they are KNOWN: the read-aloud
    /// passage, or a fixture's own baseline text - which is another
    /// model's output, and only as true as that model was. `None` for a
    /// saved take.
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
        Tier::Latency => takes(takes_dir)?,
        Tier::Consensus => {
            let mut clips = takes(takes_dir)?;
            clips.extend(fixtures()?);
            clips
        }
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

/// The repo's fixture corpus, embedded.
///
/// **A shipped binary has no repo beside it, and these clips ARE the
/// consensus tier** - so the wavs and the manifest ride in the binary the
/// way the read-aloud set rides on the machine. The manifest names each
/// file; the byte table below is what resolves a name to its own bytes,
/// and a test fails if the two ever disagree.
pub fn fixtures() -> Result<Vec<Clip>, Error> {
    #[derive(serde::Deserialize)]
    struct Entry {
        file: String,
        baseline_normalized: String,
    }

    let entries: Vec<Entry> = serde_json::from_str(include_str!("../fixtures/manifest.json"))
        .map_err(|error| Error::Bench { message: format!("the fixture manifest: {error}") })?;

    let mut clips = Vec::new();
    for entry in entries {
        let Some(bytes) = fixture_bytes(&entry.file) else {
            return Err(Error::Bench {
                message: format!("no embedded bytes for the fixture {}", entry.file),
            });
        };
        let audio = decode_wav_bytes(bytes, &entry.file)?;
        clips.push(Clip {
            source: ClipSource::Fixture,
            audio,
            truth: Some(entry.baseline_normalized),
            recorded: None,
        });
    }
    Ok(clips)
}

/// One fixture's bytes, by the manifest's own file name. A new fixture is
/// an arm here and a row in the manifest, and the pair is tested.
fn fixture_bytes(file: &str) -> Option<&'static [u8]> {
    Some(match file {
        "01_003s.wav" => include_bytes!("../fixtures/01_003s.wav"),
        "02_004s.wav" => include_bytes!("../fixtures/02_004s.wav"),
        "03_005s.wav" => include_bytes!("../fixtures/03_005s.wav"),
        "04_005s.wav" => include_bytes!("../fixtures/04_005s.wav"),
        "05_006s.wav" => include_bytes!("../fixtures/05_006s.wav"),
        "06_006s.wav" => include_bytes!("../fixtures/06_006s.wav"),
        "07_007s.wav" => include_bytes!("../fixtures/07_007s.wav"),
        "08_009s.wav" => include_bytes!("../fixtures/08_009s.wav"),
        "09_012s.wav" => include_bytes!("../fixtures/09_012s.wav"),
        "10_013s.wav" => include_bytes!("../fixtures/10_013s.wav"),
        "11_013s.wav" => include_bytes!("../fixtures/11_013s.wav"),
        "12_014s.wav" => include_bytes!("../fixtures/12_014s.wav"),
        "13_015s.wav" => include_bytes!("../fixtures/13_015s.wav"),
        "14_016s.wav" => include_bytes!("../fixtures/14_016s.wav"),
        "15_020s.wav" => include_bytes!("../fixtures/15_020s.wav"),
        _ => return None,
    })
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
        // A passage with no take under it is an arming that never got its
        // recording; the caller sees no set rather than a silent clip.
        return Ok(None);
    };
    Ok(Some(Clip { source: ClipSource::ReadAloud, truth: Some(truth), ..clip }))
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

/// Decode one 16-bit mono wav at the model's own rate, refusing anything
/// else by name rather than resampling it.
fn decode_wav(path: &Path) -> Result<Vec<f32>, Error> {
    let reader = hound::WavReader::open(path)
        .map_err(|error| Error::Bench { message: format!("{}: {error}", path.display()) })?;
    decode_reader(reader, &path.display().to_string())
}

/// [`decode_wav`] over bytes rather than a path, for the embedded corpus.
fn decode_wav_bytes(bytes: &[u8], name: &str) -> Result<Vec<f32>, Error> {
    let reader = hound::WavReader::new(Cursor::new(bytes))
        .map_err(|error| Error::Bench { message: format!("{name}: {error}") })?;
    decode_reader(reader, name)
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

    /// The committed fixture corpus reads whole, from its embedded bytes:
    /// fifteen clips, each with its baseline as the clip's truth. **The
    /// manifest and the byte table are one set** - a fixture added to one
    /// without the other fails here rather than mid-run.
    #[test]
    fn the_fixture_corpus_reads_by_its_manifest() {
        let clips = fixtures().unwrap();

        assert_eq!(clips.len(), 15, "the committed corpus is fifteen clips");
        assert!(clips.iter().all(|clip| clip.source == ClipSource::Fixture));
        assert!(
            clips.iter().all(|clip| clip.truth.is_some()),
            "every fixture carries its baseline to compare against"
        );
        assert!(clips.iter().all(|clip| !clip.audio.is_empty()));

        // The denominator: every name the manifest carries has bytes.
        let manifest: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../fixtures/manifest.json")).unwrap();
        for entry in &manifest {
            let name = entry["file"].as_str().unwrap();
            assert!(fixture_bytes(name).is_some(), "no embedded bytes for {name}");
        }
    }

    /// The corpus's identity moves with its clips: the count, the seconds,
    /// and a hash over the samples, so two takes of different audio cannot
    /// share one.
    #[test]
    fn a_corpus_id_follows_its_clips() {
        let dir = tempfile::tempdir().unwrap();
        write_take(dir.path(), 1, 16_000, "one");
        let empty = tempfile::tempdir().unwrap();

        let one = corpus(Tier::Latency, dir.path(), empty.path()).unwrap();
        let again = corpus(Tier::Latency, dir.path(), empty.path()).unwrap();
        assert_eq!(one.id, again.id, "the same clips are the same corpus");
        assert_eq!(one.id.clips, 1);
        assert_eq!(one.id.audio_seconds, 1, "16k samples is one second");

        write_take(dir.path(), 2, 8_000, "two");
        let two = corpus(Tier::Latency, dir.path(), empty.path()).unwrap();
        assert_ne!(one.id.sha256, two.id.sha256, "one more take is a different corpus");
        assert_eq!(two.id.clips, 2);

        let consensus = corpus(Tier::Consensus, dir.path(), empty.path()).unwrap();
        assert_eq!(consensus.id.clips, 2 + 15, "the takes plus the embedded corpus");

        let gold = corpus(Tier::ReadAloud, empty.path(), empty.path()).unwrap();
        assert!(gold.clips.is_empty(), "no passage recorded, no read-aloud tier");
    }
}
