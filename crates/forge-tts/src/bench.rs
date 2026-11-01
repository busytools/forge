//! The speaking bench: synthesize a corpus's transcripts, hear them back
//! through the settled recognizer, and score the round trip.
//!
//! The recognizer is passed in rather than reached for: this crate is a leaf
//! and owns no model of the other direction, so the caller supplies whatever
//! pair actually transcribes on this machine. What the bench measures is the
//! loop a person hears - words in, speech out, the same words recognized
//! again - plus how soon the first audio starts and how far the render runs
//! ahead of real time.

use std::time::{Duration, Instant};

use crate::{Engine, Error, SpeakRequest};

/// One clip to score: the words to say.
pub struct BenchClip {
    pub transcript: String,
}

/// What one run measured.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TtsBenchResult {
    pub clips: usize,
    /// Word error rate of what was heard against what was said, pooled over
    /// every clip.
    pub round_trip_wer: f64,
    /// Mean time from a call to its first audio.
    pub first_audio_ms: u64,
    /// Rendered audio seconds per wall second.
    pub realtime_factor: f64,
}

/// Synthesize every transcript, transcribe the audio back, and score it.
///
/// `on_clip` is told each clip's 1-based position as it lands; a `Break`
/// stops the run with [`Error::Cancelled`], because what ran is not a
/// result.
pub async fn run(
    engine: &dyn Engine,
    corpus: &[BenchClip],
    transcribe: impl Fn(&[f32], u32) -> String,
    mut on_clip: impl FnMut(usize) -> std::ops::ControlFlow<()>,
) -> Result<TtsBenchResult, Error> {
    if corpus.is_empty() {
        return Err(Error::Render("the corpus has no clips to run".to_owned()));
    }
    let started_at = Instant::now();
    let mut first_audio = Duration::ZERO;
    let mut audio_seconds = 0.0_f64;
    let mut edits_total = 0_usize;
    let mut words_total = 0_usize;

    for (index, clip) in corpus.iter().enumerate() {
        let started = Instant::now();
        let mut chunks = engine.speak(SpeakRequest {
            text: clip.transcript.clone(),
            language: None,
            speaker: None,
        });
        let mut samples: Vec<f32> = Vec::new();
        let mut rate = 0_u32;
        let mut first: Option<Duration> = None;
        while let Some(chunk) = chunks.recv().await {
            let chunk = chunk?;
            if first.is_none() {
                first = Some(started.elapsed());
                rate = chunk.sample_rate;
            }
            samples.extend(chunk.pcm.iter().map(|sample| f32::from(*sample) / f32::from(i16::MAX)));
        }
        let Some(first) = first else {
            // Nothing rendered is nothing to score, and the mean latency it
            // would add is a number about an empty clip.
            continue;
        };
        first_audio += first;
        if rate > 0 {
            let count = u32::try_from(samples.len()).unwrap_or(u32::MAX);
            audio_seconds += f64::from(count) / f64::from(rate);
        }
        let heard = transcribe(&samples, rate);
        let truth = words(&clip.transcript);
        edits_total += edits(&words(&heard), &truth);
        words_total += truth.len();
        if on_clip(index + 1) == std::ops::ControlFlow::Break(()) {
            return Err(Error::Cancelled);
        }
    }

    let wall = started_at.elapsed().as_secs_f64();
    Ok(TtsBenchResult {
        clips: corpus.len(),
        round_trip_wer: ratio(edits_total, words_total),
        first_audio_ms: u64::try_from(first_audio.as_millis()).unwrap_or(u64::MAX)
            / u64::try_from(corpus.len()).unwrap_or(1),
        realtime_factor: if wall > 0.0 { audio_seconds / wall } else { 0.0 },
    })
}

/// A string's words: lowercased, split on anything that is not a letter or a
/// digit, blanks dropped.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Edits over the truth's own words; a truth with no words is no error
/// rather than a division by zero.
fn ratio(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    f64::from(u32::try_from(part).unwrap_or(u32::MAX))
        / f64::from(u32::try_from(whole).unwrap_or(u32::MAX))
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

#[cfg(test)]
mod tests {
    use super::{BenchClip, run};
    use crate::{Chunk, Engine, SpeakRequest};
    use tokio::sync::mpsc;

    /// A render that answers one clip, so the bench's figures are its own
    /// rather than a model's.
    struct StubEngine;

    impl Engine for StubEngine {
        fn speak(&self, _request: SpeakRequest) -> mpsc::Receiver<Result<Chunk, crate::Error>> {
            let (sender, receiver) = mpsc::channel(2);
            tokio::spawn(async move {
                let _ = sender
                    .send(Ok(Chunk { seq: 0, sample_rate: 24_000, pcm: vec![1; 2_400] }))
                    .await;
            });
            receiver
        }
    }

    fn clips() -> Vec<BenchClip> {
        vec![BenchClip { transcript: "the quick brown fox".to_owned() }]
    }

    #[tokio::test]
    async fn the_tts_bench_reports_wer_and_latency_off_a_stub_engine() {
        let result = run(
            &StubEngine,
            &clips(),
            |_, _| "the quick brown fox".to_owned(),
            |_| std::ops::ControlFlow::Continue(()),
        )
        .await
        .expect("the run lands");
        assert_eq!(result.clips, 1);
        assert_eq!(result.round_trip_wer, 0.0, "the exact words came back");
        assert!(result.realtime_factor > 0.0, "a render with audio has a pace");
    }

    #[tokio::test]
    async fn a_round_trip_that_changes_a_word_scores_an_error() {
        let result = run(
            &StubEngine,
            &clips(),
            |_, _| "the quick brown fix".to_owned(),
            |_| std::ops::ControlFlow::Continue(()),
        )
        .await
        .expect("the run lands");
        assert!(result.round_trip_wer > 0.0, "a changed word is an edit");
    }

    #[tokio::test]
    async fn an_empty_corpus_is_refused_by_name() {
        let Err(error) = run(&StubEngine, &[], |_, _| String::new(), |_| {
            std::ops::ControlFlow::Continue(())
        })
        .await
        else {
            panic!("an empty corpus is no run");
        };
        assert!(error.to_string().contains("no clips"));
    }

    #[tokio::test]
    async fn each_scored_clip_is_reported_as_it_lands() {
        let corpus = vec![
            BenchClip { transcript: "one".to_owned() },
            BenchClip { transcript: "two".to_owned() },
        ];
        let mut landed = Vec::new();
        run(&StubEngine, &corpus, |_, _| "one".to_owned(), |index| {
            landed.push(index);
            std::ops::ControlFlow::Continue(())
        })
        .await
        .expect("the run lands");

        assert_eq!(landed, vec![1, 2], "the caller hears every clip, in order");
    }

    #[tokio::test]
    async fn a_stopped_run_is_cancelled_and_scores_nothing() {
        let corpus = vec![
            BenchClip { transcript: "one".to_owned() },
            BenchClip { transcript: "two".to_owned() },
        ];
        let Err(error) = run(&StubEngine, &corpus, |_, _| "one".to_owned(), |_| {
            std::ops::ControlFlow::Break(())
        })
        .await
        else {
            panic!("a stopped run is not a result");
        };
        assert!(matches!(error, crate::Error::Cancelled));
    }
}
