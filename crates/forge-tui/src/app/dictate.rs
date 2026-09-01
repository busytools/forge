//! Composer dictation indicator: per-session take state, the level
//! ramp the top border renders, and the post-take notice wording.

use std::time::{Duration, Instant};

use ratatui::style::Style;
use ratatui::text::Span;

use crate::app::App;
use crate::ui::theme;

/// Silence before a transcribing composer says it is waiting. The warm
/// path resolves in well under this; only a cold start ever draws.
pub(crate) const TRANSCRIBING_INDICATOR_MS: u128 = 3000;

/// The block ramp for the level cells, floor to full scale.
pub(crate) const LEVEL_RAMP: [char; 8] = [
    '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}',
];

/// The active take on one session's composer. Lives on the bucket, so
/// a take started in one session never renders in another.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DictateIndicator {
    pub(crate) floor_db: f32,
    pub(crate) phase: DictatePhase,
    /// Last three window peaks, oldest first.
    pub(crate) levels: [f32; 3],
    pub(crate) transcribing_since: Option<Instant>,
    /// This take's generation, as the workspace handed out at start. A
    /// resolver for a smaller number belongs to an older take of the
    /// same key and resets nothing.
    pub(crate) generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DictatePhase {
    Recording,
    Transcribing,
}

/// The one notice row the box shows after a take resolves. Lives on
/// the bucket (not on [`DictateIndicator`]) because it outlives the
/// take that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DictateNotice {
    pub(crate) severity: NoticeSeverity,
    pub(crate) text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoticeSeverity {
    Dim,
    Warn,
    Error,
}

impl DictateIndicator {
    pub(crate) fn recording(floor_db: f32, generation: u64) -> Self {
        Self {
            floor_db,
            phase: DictatePhase::Recording,
            levels: [f32::NEG_INFINITY; 3],
            transcribing_since: None,
            generation,
        }
    }

    /// One 50 ms window peak; the oldest falls off.
    pub(crate) fn push_level(&mut self, peak_db: f32) {
        self.levels.rotate_left(1);
        self.levels[2] = peak_db;
    }

    pub(crate) fn begin_transcribing(&mut self) {
        self.phase = DictatePhase::Transcribing;
        self.transcribing_since = Some(Instant::now());
    }

    /// Past the silence threshold, so the pinned cell and the esc hint
    /// may draw. Before it the box shows nothing at all.
    pub(crate) fn transcribing_overdue(&self) -> bool {
        self.transcribing_since
            .is_some_and(|since| since.elapsed().as_millis() >= TRANSCRIBING_INDICATOR_MS)
    }
}

/// Map a window peak onto the ramp. The floor glyph is zero: a bar
/// that never leaves the floor is what `Outcome::NoAudio` later
/// reports, so the meter and the verdict agree by construction.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn level_cell(peak_db: f32, floor_db: f32) -> char {
    if !peak_db.is_finite() || peak_db <= floor_db {
        return LEVEL_RAMP[0];
    }
    // Clamped to [0, 1] first, so the rounded step stays inside the ramp.
    let frac = ((peak_db - floor_db) / -floor_db).clamp(0.0, 1.0);
    LEVEL_RAMP[(frac * 7.0).round() as usize]
}

/// The pinned `bars_v` frame for the transcribing cell, at that
/// style's own cadence whatever the active `/spinner` choice.
pub(crate) fn transcribing_cell(elapsed: Duration) -> char {
    let frames = forge_workspace::SpinnerStyle::BarsV.frames();
    let cadence = u128::from(forge_workspace::SpinnerStyle::BarsV.cadence_ms());
    frames[(elapsed.as_millis() / cadence) as usize % frames.len()]
}

/// The notice a finished take leaves, worded per outcome. A finite
/// quiet-room peak quotes its own measurement; structural silence
/// offers no retry because it is sticky within the process.
pub(crate) fn notice_for_outcome(
    outcome: &forge_workspace::DictateOutcome,
    floor_db: f32,
) -> Option<DictateNotice> {
    match outcome {
        forge_workspace::DictateOutcome::Landed { truncated: false, .. }
        | forge_workspace::DictateOutcome::Cancelled => None,
        forge_workspace::DictateOutcome::Landed { truncated: true, .. } => Some(DictateNotice {
            severity: NoticeSeverity::Warn,
            text: "this is what fitted \u{b7} keep going from the end".to_owned(),
        }),
        forge_workspace::DictateOutcome::Empty => Some(DictateNotice {
            severity: NoticeSeverity::Dim,
            text: "that was all filler \u{b7} nothing to insert".to_owned(),
        }),
        forge_workspace::DictateOutcome::NoAudio { peak_db, seconds } if peak_db.is_finite() => {
            Some(DictateNotice {
                severity: NoticeSeverity::Dim,
                text: format!(
                    "nothing above {} dBFS in {seconds}s \u{b7} loudest was {peak_db:.1} \u{b7} try again",
                    floor_db.round()
                ),
            })
        }
        forge_workspace::DictateOutcome::NoAudio { .. } => Some(DictateNotice {
            severity: NoticeSeverity::Error,
            text: "no signal from the microphone at all \u{b7} check permission or mute".to_owned(),
        }),
        forge_workspace::DictateOutcome::Refused { message } => {
            Some(DictateNotice { severity: NoticeSeverity::Error, text: message.clone() })
        }
        forge_workspace::DictateOutcome::Failed => Some(DictateNotice {
            severity: NoticeSeverity::Dim,
            text: "dictation failed \u{b7} try again; restart forge if it repeats".to_owned(),
        }),
    }
}

/// Whether the active composer has a live take, which is what gives
/// Esc its discard/abandon meaning.
pub(crate) fn dictate_owns_esc(app: &App) -> bool {
    app.active_session().is_some_and(|bucket| bucket.dictate.is_some())
}

/// Whether the composer currently renders its one notice row: a
/// stamped post-take notice, or the esc hint once a transcription is
/// past the silence threshold. Drives both the render and the layout
/// height so the two never disagree.
pub(crate) fn notice_row_visible(app: &App) -> bool {
    let Some(bucket) = app.active_session() else { return false };
    if bucket.visible_dictate_notice().is_some() {
        return true;
    }
    bucket
        .dictate
        .as_ref()
        .is_some_and(|d| d.phase == DictatePhase::Transcribing && d.transcribing_overdue())
}

/// The top-border indicator cells for the active composer: three
/// cells idle and while recording, one pinned `bars_v` cell once
/// transcribing is past the silence threshold, and nothing at all
/// before it. `None` leaves the plain border.
pub(crate) fn indicator_spans(app: &App) -> Option<Vec<Span<'static>>> {
    if !app.dictate_available {
        return None;
    }
    let bucket = app.active_session()?;
    match bucket.dictate.as_ref() {
        None => {
            Some(vec![Span::styled("\u{2581}\u{2581}\u{2581}", Style::default().fg(theme::DIM))])
        }
        Some(indicator) => match indicator.phase {
            DictatePhase::Recording => Some(
                indicator
                    .levels
                    .iter()
                    .map(|peak| {
                        let style = if *peak > indicator.floor_db {
                            Style::default().fg(theme::RUST_ORANGE)
                        } else {
                            Style::default().fg(theme::DIM)
                        };
                        Span::styled(level_cell(*peak, indicator.floor_db).to_string(), style)
                    })
                    .collect(),
            ),
            DictatePhase::Transcribing => {
                let since = indicator.transcribing_since?;
                if !indicator.transcribing_overdue() {
                    return None;
                }
                Some(vec![Span::styled(
                    transcribing_cell(since.elapsed()).to_string(),
                    Style::default().fg(theme::RUST_ORANGE),
                )])
            }
        },
    }
}

/// Colour for a stamped notice's severity.
pub(crate) fn notice_style(severity: NoticeSeverity) -> Style {
    match severity {
        NoticeSeverity::Dim => Style::default().fg(theme::DIM),
        NoticeSeverity::Warn => Style::default().fg(theme::STATUS_WARNING),
        NoticeSeverity::Error => Style::default().fg(theme::STATUS_ERROR),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_workspace::DictateOutcome;

    #[test]
    fn level_history_keeps_the_last_three_windows() {
        let mut indicator = DictateIndicator::recording(-50.0, 1);
        assert!(
            indicator.levels.iter().all(|l| l.is_infinite() && l.is_sign_negative()),
            "a fresh take starts on the floor"
        );
        indicator.push_level(-30.0);
        indicator.push_level(-20.0);
        indicator.push_level(-10.0);
        indicator.push_level(-5.0);
        assert!(
            (indicator.levels[0] + 20.0).abs() < f32::EPSILON,
            "oldest of the surviving windows first, got {}",
            indicator.levels[0]
        );
        assert!(
            (indicator.levels[2] + 5.0).abs() < f32::EPSILON,
            "newest last, got {}",
            indicator.levels[2]
        );
    }

    #[test]
    fn the_ramp_maps_the_configured_floor_to_zero() {
        assert_eq!(level_cell(f32::NEG_INFINITY, -50.0), LEVEL_RAMP[0], "no signal is the floor");
        assert_eq!(level_cell(-60.0, -50.0), LEVEL_RAMP[0], "below the floor is the floor");
        assert_eq!(level_cell(-50.0, -50.0), LEVEL_RAMP[0], "at the floor is the floor");
        assert_eq!(level_cell(0.0, -50.0), LEVEL_RAMP[7], "full scale is the full block");
        assert_eq!(
            level_cell(-25.0, -50.0),
            LEVEL_RAMP[4],
            "halfway between floor and full scale is the middle step"
        );
    }

    #[test]
    fn transcribing_stays_silent_for_the_first_three_seconds() {
        let mut indicator = DictateIndicator::recording(-50.0, 1);
        indicator.phase = DictatePhase::Transcribing;
        indicator.transcribing_since = Some(
            Instant::now()
                .checked_sub(Duration::from_millis(2999))
                .expect("a 3 s backdate is safe"),
        );
        assert!(!indicator.transcribing_overdue(), "the warm case must never reach the indicator");
        indicator.transcribing_since = Some(
            Instant::now()
                .checked_sub(Duration::from_millis(3001))
                .expect("a 3 s backdate is safe"),
        );
        assert!(indicator.transcribing_overdue(), "a cold start does reach it");
    }

    #[test]
    fn the_transcribing_cell_walks_bars_v_at_its_own_cadence() {
        assert_eq!(
            transcribing_cell(Duration::from_millis(0)),
            transcribing_cell(Duration::from_millis(1)),
            "within one cadence step the frame holds"
        );
        assert_ne!(
            transcribing_cell(Duration::from_millis(0)),
            transcribing_cell(Duration::from_millis(280)),
            "four cadence steps in, the frame has moved"
        );
        assert_eq!(
            transcribing_cell(Duration::from_millis(0)),
            transcribing_cell(Duration::from_millis(70 * 14)),
            "the ramp wraps after one full cycle"
        );
    }

    #[test]
    fn notices_distinguish_quiet_from_structural_silence() {
        let quiet =
            notice_for_outcome(&DictateOutcome::NoAudio { peak_db: -38.2, seconds: 4 }, -50.0);
        let quiet = quiet.expect("a quiet room says so");
        assert!(
            quiet.text.contains("-50")
                && quiet.text.contains("-38.2")
                && quiet.text.contains("try again"),
            "a quiet room quotes the floor, its own peak and a retry: {}",
            quiet.text
        );

        let structural = notice_for_outcome(
            &DictateOutcome::NoAudio { peak_db: f32::NEG_INFINITY, seconds: 4 },
            -50.0,
        )
        .expect("structural silence says so");
        assert!(
            !structural.text.contains("try again"),
            "structural silence is sticky, so no retry is offered: {}",
            structural.text
        );
        assert_eq!(
            notice_for_outcome(
                &DictateOutcome::Landed { text: "hi".to_owned(), truncated: false },
                -50.0
            ),
            None,
            "landed words need no notice"
        );
        let truncated = notice_for_outcome(
            &DictateOutcome::Landed { text: "hi".to_owned(), truncated: true },
            -50.0,
        )
        .expect("a truncated take says so");
        assert_eq!(truncated.severity, NoticeSeverity::Warn, "partial words are a warning");
    }
}
