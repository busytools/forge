//! Composer dictation surfaces: per-session take state, the normalized
//! level meter behind the status row, the border handoff palette, and
//! the post-take notice wording.

use std::time::{Duration, Instant};

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::app::App;
use crate::app::session::UiSession;
use crate::ui::theme;

/// Silence before a transcribing composer says it is waiting. The warm
/// path resolves in well under this; only a cold start ever draws.
pub(crate) const TRANSCRIBING_INDICATOR_MS: u128 = 3000;

/// The block ramp for the meter cells, floor to full scale.
pub(crate) const LEVEL_RAMP: [char; 8] = [
    '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}',
];

/// Meter cells in the status row.
pub(crate) const METER_WIDTH: usize = 26;

/// Envelope, reference and gate constants are per 50 ms tick, the
/// cadence `DictateLevel` arrives on.
const ATTACK: f32 = 0.6;
const RELEASE: f32 = 0.25;
const REFERENCE_DECAY_DB: f32 = 0.7;
const MIN_SPAN_DB: f32 = 6.0;
const GAMMA: f32 = 0.9;
/// Floor the raw feed stands in at for structural silence, so the
/// envelope arithmetic stays finite.
const SILENCE_DB: f32 = -52.0;
/// Envelope and reference start points: under the gate, with an
/// autosens prior a quiet first syllable can still rise against.
const START_ENV_DB: f32 = -50.0;
const START_REFERENCE_DB: f32 = -30.0;

// The v3 handoff palette. Blue and green match the review accents'
// values but carry dictate semantics, so they are named here rather
// than borrowed from `theme`.
const ORANGE: [f32; 3] = [244.0, 118.0, 0.0];
const HOT: [f32; 3] = [255.0, 176.0, 88.0];
const BLUE: [f32; 3] = [97.0, 160.0, 224.0];
const GREEN: [f32; 3] = [130.0, 199.0, 107.0];
const METER_LOW: [f32; 3] = [64.0, 68.0, 76.0];
const CANVAS: [f32; 3] = [23.0, 25.0, 30.0];

/// Dot pulse period and dimmest opacity, from the mock's CSS.
const PULSE_PERIOD_MS: f32 = 1050.0;
const PULSE_FLOOR: f32 = 0.3;
/// Border easing per 50 ms tick toward the state's target colour.
const BORDER_EASE_PER_TICK: f32 = 0.12;
/// How long the green done-beat holds before easing back to orange.
pub(crate) const GREEN_BEAT: Duration = Duration::from_millis(450);
/// The cursor dB figure refresh cadence - 5 Hz, whatever the frame
/// rate.
const DB_READOUT_MS: u128 = 200;
/// A meter cell at or under this fraction draws the gate's floor
/// glyph in DIM.
const FLOOR_FRAC: f32 = 0.02;

fn mix3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

fn colour_distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    (a[0] - b[0]).abs() + (a[1] - b[1]).abs() + (a[2] - b[2]).abs()
}

fn rgbf(c: [f32; 3]) -> Color {
    fn channel(v: f32) -> u8 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            v.round().clamp(0.0, 255.0) as u8
        }
    }
    Color::Rgb(channel(c[0]), channel(c[1]), channel(c[2]))
}

/// One envelope step in the dB domain: fast attack, slower release.
fn envelope_step(env_db: f32, raw_db: f32) -> f32 {
    let rate = if raw_db > env_db { ATTACK } else { RELEASE };
    env_db + (raw_db - env_db) * rate
}

/// The rolling reference decays slowly and never drops below the
/// envelope, so it is the take's own loudness the meter scales against.
fn reference_step(env_db: f32, reference_db: f32) -> f32 {
    env_db.max(reference_db - REFERENCE_DECAY_DB)
}

/// Gate, scale against the reference span, then soften with the mock's
/// gamma. Anything at or under the take's own silence floor is
/// structurally zero - the same condition `Outcome::NoAudio` reports,
/// so the meter and the verdict agree by construction.
fn normalize(env_db: f32, reference_db: f32, floor_db: f32) -> f32 {
    if !env_db.is_finite() || env_db <= floor_db {
        return 0.0;
    }
    let span = (reference_db - floor_db).max(MIN_SPAN_DB);
    (((env_db - floor_db) / span).clamp(0.0, 1.0)).powf(GAMMA)
}

/// Map a normalized fraction onto the ramp. The floor glyph is zero: a
/// bar that never leaves the floor is what `Outcome::NoAudio` later
/// reports, so the meter and the verdict agree by construction.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn level_cell(frac: f32) -> char {
    if !frac.is_finite() {
        return LEVEL_RAMP[0];
    }
    LEVEL_RAMP[(frac.clamp(0.0, 1.0) * 7.0).round() as usize]
}

/// The TUI-side normalized meter. `forge-dictate` stays host-blind: it
/// keeps feeding per-window `peak_db`, and everything display-side -
/// envelope ballistics, autosens reference, noise gate - lives here.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DictateMeter {
    floor_db: f32,
    env_db: f32,
    reference_db: f32,
    /// Newest-last sliding window of normalized fractions.
    levels: Vec<f32>,
}

impl DictateMeter {
    fn new(floor_db: f32) -> Self {
        Self {
            floor_db,
            env_db: START_ENV_DB,
            reference_db: START_REFERENCE_DB,
            levels: vec![0.0; METER_WIDTH],
        }
    }

    /// One 50 ms window peak; the oldest normalized frame falls off.
    pub(crate) fn push(&mut self, peak_db: f32) {
        let raw = peak_db.max(SILENCE_DB);
        self.env_db = envelope_step(self.env_db, raw);
        self.reference_db = reference_step(self.env_db, self.reference_db);
        let frac = normalize(self.env_db, self.reference_db, self.floor_db);
        if self.levels.len() >= METER_WIDTH {
            self.levels.remove(0);
        }
        self.levels.push(frac);
    }

    pub(crate) fn window(&self) -> &[f32] {
        &self.levels
    }

    /// The newest frame, which is what the border's level ride and the
    /// dB readout colour follow.
    pub(crate) fn current(&self) -> f32 {
        self.levels.last().copied().unwrap_or(0.0)
    }

    pub(crate) fn env_db(&self) -> f32 {
        self.env_db
    }
}

/// The active take on one session's composer. Lives on the bucket, so
/// a take started in one session never renders in another.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DictateIndicator {
    pub(crate) floor_db: f32,
    pub(crate) phase: DictatePhase,
    pub(crate) meter: DictateMeter,
    /// Wall clock the take started at; the mm:ss timer divides it.
    pub(crate) started: Instant,
    /// Elapsed recording time, frozen when transcription begins.
    pub(crate) recording_duration: Option<Duration>,
    pub(crate) transcribing_since: Option<Instant>,
    /// The cursor spot's held dB figure and when it was stamped.
    db_shown: f32,
    db_shown_at: Instant,
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

impl DictateIndicator {
    pub(crate) fn recording(floor_db: f32, generation: u64) -> Self {
        let now = Instant::now();
        Self {
            floor_db,
            phase: DictatePhase::Recording,
            meter: DictateMeter::new(floor_db),
            started: now,
            recording_duration: None,
            transcribing_since: None,
            db_shown: START_ENV_DB,
            db_shown_at: now,
            generation,
        }
    }

    /// One 50 ms window peak, while recording. A reading that races
    /// past the handoff must not move the frozen frame.
    pub(crate) fn push_level(&mut self, peak_db: f32) {
        if self.phase != DictatePhase::Recording {
            return;
        }
        self.meter.push(peak_db);
    }

    pub(crate) fn begin_transcribing(&mut self) {
        if self.phase == DictatePhase::Recording {
            self.recording_duration = Some(self.started.elapsed());
        }
        self.phase = DictatePhase::Transcribing;
        self.transcribing_since = Some(Instant::now());
    }

    /// The mm:ss figure the status row shows: live while recording,
    /// frozen at the take's length once transcription begins.
    pub(crate) fn take_elapsed(&self) -> Duration {
        match self.phase {
            DictatePhase::Recording => self.started.elapsed(),
            DictatePhase::Transcribing => self.recording_duration.unwrap_or_default(),
        }
    }

    /// The cursor spot's dB figure and its colour level, throttled to
    /// 5 Hz display-side. Between refreshes the held figure stands.
    pub(crate) fn db_readout(&mut self, now: Instant) -> (f32, f32) {
        if now.duration_since(self.db_shown_at).as_millis() >= DB_READOUT_MS {
            self.db_shown = self.meter.env_db();
            self.db_shown_at = now;
        }
        (self.db_shown, self.meter.current())
    }

    /// Past the silence threshold, so the status row may draw. Before
    /// it the box shows nothing at all.
    pub(crate) fn transcribing_overdue(&self) -> bool {
        self.transcribing_since
            .is_some_and(|since| since.elapsed().as_millis() >= TRANSCRIBING_INDICATOR_MS)
    }
}

/// The composer border's dictate state for one session: the eased
/// colour while a take is live, then a frozen snapshot the afterglow
/// computes from, so nothing on a background bucket needs a render
/// visit to die.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DictateBorder {
    Live { rgb: [f32; 3], last_step: Instant },
    Afterglow { started: Instant, rgb: [f32; 3], beat: bool },
}

impl DictateBorder {
    /// A new take carries over wherever the border currently sits, so
    /// a quick re-dictate never snaps.
    pub(crate) fn live(previous: Option<[f32; 3]>, now: Instant) -> Self {
        Self::Live { rgb: previous.unwrap_or(ORANGE), last_step: now }
    }

    pub(crate) fn rgb(&self) -> [f32; 3] {
        match self {
            Self::Live { rgb, .. } | Self::Afterglow { rgb, .. } => *rgb,
        }
    }

    /// Whether the border still owes frames: a live take always, an
    /// afterglow only until its colour is analytically home.
    pub(crate) fn animating(&self, now: Instant) -> bool {
        match self {
            Self::Live { .. } => true,
            Self::Afterglow { .. } => afterglow_colour(self, now).is_some(),
        }
    }
}

/// The border target per take state. Recording rides the level toward
/// the hot tint; transcription hands off to blue; with no take the
/// afterglow easing carries the target instead.
fn border_target(bucket: &UiSession) -> [f32; 3] {
    match bucket.dictate.as_ref() {
        Some(indicator) => match indicator.phase {
            DictatePhase::Recording => mix3(ORANGE, HOT, indicator.meter.current() * 0.35),
            DictatePhase::Transcribing => BLUE,
        },
        None => ORANGE,
    }
}

/// One eased step toward `to_db`, 0.12 per 50 ms of elapsed time.
fn eased(from: [f32; 3], to: [f32; 3], elapsed_ms: f32) -> [f32; 3] {
    let ticks = (elapsed_ms / 50.0).max(1.0);
    let t = 1.0 - (1.0 - BORDER_EASE_PER_TICK).powf(ticks);
    mix3(from, to, t)
}

/// The afterglow's colour, computed purely from its own snapshot: a
/// landed take beats green for the beat window, then everything eases
/// home. `None` once back within a colour step of the composer's
/// orange, which is how the state expires without a render visit.
fn afterglow_colour(border: &DictateBorder, now: Instant) -> Option<[f32; 3]> {
    let DictateBorder::Afterglow { started, rgb, beat } = border else {
        return Some(border.rgb());
    };
    let elapsed_ms = now.saturating_duration_since(*started).as_secs_f32() * 1000.0;
    let beat_ms = GREEN_BEAT.as_secs_f32() * 1000.0;
    let (from, to, step_ms) = if *beat && elapsed_ms < beat_ms {
        (*rgb, GREEN, elapsed_ms)
    } else {
        let tail_ms = if *beat { elapsed_ms - beat_ms } else { elapsed_ms };
        (if *beat { GREEN } else { *rgb }, ORANGE, tail_ms)
    };
    let colour = eased(from, to, step_ms);
    (colour_distance(colour, ORANGE) >= 0.5).then_some(colour)
}

/// Advance the active composer's border easing one render and return
/// the colour to draw it in, or `None` when no handoff is in flight
/// and the plain orange must stand.
pub(crate) fn border_color(app: &mut App, now: Instant) -> Option<Color> {
    let bucket = app.try_active_bucket_mut()?;
    if bucket.dictate_border.as_ref().is_some_and(|border| matches!(border, DictateBorder::Live { .. })) {
        let target = border_target(bucket);
        if let Some(DictateBorder::Live { rgb, last_step }) = bucket.dictate_border.as_mut() {
            let elapsed_ms = now.duration_since(*last_step).as_secs_f32() * 1000.0;
            *last_step = now;
            *rgb = eased(*rgb, target, elapsed_ms);
            if bucket.dictate.is_none() && colour_distance(*rgb, ORANGE) < 0.5 {
                bucket.dictate_border = None;
                return None;
            }
            return Some(rgbf(*rgb));
        }
    }
    let Some(colour) = afterglow_colour(bucket.dictate_border.as_ref()?, now) else {
        bucket.dictate_border = None;
        return None;
    };
    Some(rgbf(colour))
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

/// Whether the composer renders its one interior row: a stamped
/// post-take notice, or the status row while a take is live - always
/// while recording, and only past the silence threshold while
/// transcribing. Drives both the render and the layout height so the
/// two never disagree.
pub(crate) fn dictate_row_visible(app: &App) -> bool {
    let Some(bucket) = app.active_session() else { return false };
    if bucket.visible_dictate_notice().is_some() {
        return true;
    }
    match bucket.dictate.as_ref() {
        None => false,
        Some(indicator) => match indicator.phase {
            DictatePhase::Recording => true,
            DictatePhase::Transcribing => indicator.transcribing_overdue(),
        },
    }
}

/// The row's content: a stamped post-take notice when present, else
/// the live take's status row. The two never share the slot.
pub(crate) fn dictate_row_content(app: &App, width: usize) -> Line<'static> {
    let Some(bucket) = app.active_session() else { return Line::default() };
    if let Some(notice) = bucket.visible_dictate_notice() {
        return Line::from(Span::styled(
            format!("  {}", notice.text),
            notice_style(notice.severity),
        ));
    }
    let Some(indicator) = bucket.dictate.as_ref() else { return Line::default() };
    status_row(
        indicator,
        width,
        app.config.prefers_reduced_motion_effective(),
        app.spinner_epoch.elapsed().as_secs_f32() * 1000.0,
    )
}

/// The status row for a live take: indicator dot, mm:ss timer, label,
/// meter, right-aligned esc hint. Both live states share the anatomy;
/// only colour and freeze change on the handoff.
fn status_row(
    indicator: &DictateIndicator,
    width: usize,
    reduced_motion: bool,
    pulse_ms: f32,
) -> Line<'static> {
    let recording = indicator.phase == DictatePhase::Recording;
    let dot_glyph = if recording { "\u{25cf}" } else { "\u{25cc}" };
    let dot_base = if recording { ORANGE } else { BLUE };
    let timer_text = format_clock(indicator.take_elapsed());
    let label = if recording { "listening " } else { "transcribing " };
    let esc = "esc cancel";

    let prefix_len = 2 + 1 + 1 + timer_text.chars().count() + 1 + label.chars().count();
    let space = width.saturating_sub(prefix_len + esc.len());
    let meter_len = METER_WIDTH.min(space.saturating_sub(1));
    let pad = space - meter_len;

    let alpha = if reduced_motion {
        1.0
    } else {
        let t = (pulse_ms % PULSE_PERIOD_MS) / PULSE_PERIOD_MS;
        let wave = (1.0 - (2.0 * std::f32::consts::PI * t).cos()) / 2.0;
        PULSE_FLOOR + (1.0 - PULSE_FLOOR) * (1.0 - wave)
    };
    let dot_colour = rgbf(mix3(CANVAS, dot_base, alpha));

    let mut spans = vec![
        Span::raw("  "),
        Span::styled(dot_glyph.to_owned(), Style::default().fg(dot_colour)),
        Span::raw(" "),
        Span::styled(
            timer_text,
            Style::default().fg(if recording { theme::RUST_ORANGE } else { theme::DIM }),
        ),
        Span::raw(" "),
        Span::styled(label.to_owned(), Style::default().fg(theme::DIM)),
    ];
    // A narrow interior truncates the meter from the left: the right
    // edge is always the newest frame.
    spans.extend(indicator.meter.window().iter().rev().take(meter_len).rev().map(|frac| {
        Span::styled(
            level_cell(*frac).to_string(),
            Style::default().fg(meter_cell_colour(*frac, indicator.phase)),
        )
    }));
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(esc.to_owned(), Style::default().fg(theme::DIM)));
    Line::from(spans)
}

fn meter_cell_colour(frac: f32, phase: DictatePhase) -> Color {
    if frac <= FLOOR_FRAC {
        return theme::DIM;
    }
    match phase {
        DictatePhase::Recording => {
            let warm = mix3(METER_LOW, ORANGE, 0.35 + 0.65 * frac);
            rgbf(mix3(warm, HOT, frac * 0.5))
        }
        DictatePhase::Transcribing => {
            let base = mix3(METER_LOW, BLUE, 0.25 + 0.45 * frac);
            rgbf(mix3(CANVAS, base, 0.45))
        }
    }
}

fn format_clock(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// The cursor spot's live dB readout while recording: the text to
/// paint at the caret and its colour. `None` outside a recording, so
/// the normal blinking cursor stands.
pub(crate) fn active_db_readout(app: &mut App, now: Instant) -> Option<(String, Color)> {
    let bucket = app.try_active_bucket_mut()?;
    let indicator = bucket.dictate.as_mut()?;
    if indicator.phase != DictatePhase::Recording {
        return None;
    }
    // The readout paints the cells after the caret, so it only shows
    // at end-of-draft, where the design depicts it; mid-draft the
    // blinking cursor stands and the draft keeps its text.
    let (row, col) = bucket.input.cursor();
    let last_row = bucket.input.lines().len().saturating_sub(1);
    let last_col = bucket.input.lines().last().map_or(0, |line| line.chars().count());
    if row != last_row || col != last_col {
        return None;
    }
    let (db, _) = indicator.db_readout(now);
    // Rounded to whole dB; the environment sits within (-100, 0), so
    // the value is a small integer already.
    #[allow(clippy::cast_possible_truncation)]
    let whole_db = db.round() as i64;
    let text = format!("{whole_db} dB");
    let level = indicator.meter.current();
    let colour = if level > FLOOR_FRAC {
        rgbf(mix3(mix3(METER_LOW, ORANGE, 0.4 + 0.6 * level), HOT, level * 0.5))
    } else {
        theme::DIM
    };
    Some((text, colour))
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
    use crate::app::events::apply_session_update;
    use forge_workspace::{DictateOutcome, SessionUpdate};

    fn rgb_distance(a: Color, b: Color) -> f32 {
        let (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) = (a, b) else {
            panic!("the border tests compare rgb colours, got {a:?} vs {b:?}");
        };
        let dr = f32::from(i16::from(ar) - i16::from(br));
        let dg = f32::from(i16::from(ag) - i16::from(bg));
        let db = f32::from(i16::from(ab) - i16::from(bb));
        dr.abs() + dg.abs() + db.abs()
    }

    fn same_colour(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b.iter()).all(|(x, y)| (x - y).abs() < f32::EPSILON)
    }

    #[test]
    fn the_noise_gate_holds_everything_below_the_floor() {
        let floor = -46.0;
        assert!(normalize(-52.0, -30.0, floor).abs() < f32::EPSILON, "below the gate is silence");
        assert!(normalize(floor, -30.0, floor).abs() < f32::EPSILON, "at the gate is silence");
        assert!(
            normalize(f32::NEG_INFINITY, -30.0, floor).abs() < f32::EPSILON,
            "no signal is silence"
        );
        assert!(normalize(-40.0, -30.0, floor) > 0.0, "above the gate has signal");
    }

    #[test]
    fn attack_is_faster_than_release() {
        let rise = envelope_step(START_ENV_DB, -18.0) - START_ENV_DB;
        let fall = -18.0 - envelope_step(-18.0, START_ENV_DB);
        assert!(
            rise > fall,
            "one tick toward a loud target gains more than the symmetric tick loses: \
             rose {rise:.2} dB, fell {fall:.2} dB"
        );
    }

    #[test]
    fn the_rolling_reference_decays_slowly_and_never_drops_below_the_envelope() {
        let decayed = reference_step(-40.0, -30.0);
        assert!(
            (decayed - (-30.0 - REFERENCE_DECAY_DB)).abs() < 1e-5,
            "one tick decays exactly the reference step, got {decayed}"
        );
        let floored = reference_step(-20.0, -30.0);
        assert!((floored + 20.0).abs() < f32::EPSILON, "the envelope floors the reference");
        assert!(
            reference_step(-40.0, -30.0) > -38.0,
            "a second of decay is under 14 dB, so quiet speech still scales against the reference"
        );
    }

    #[test]
    fn the_ramp_maps_the_ends_and_the_midpoint() {
        assert_eq!(level_cell(0.0), LEVEL_RAMP[0], "zero is the floor glyph");
        assert_eq!(level_cell(1.0), LEVEL_RAMP[7], "full scale is the full block");
        assert_eq!(level_cell(0.5), LEVEL_RAMP[4], "halfway is the middle step");
        assert_eq!(level_cell(f32::NEG_INFINITY), LEVEL_RAMP[0], "non-finite is the floor");
        assert_eq!(level_cell(2.0), LEVEL_RAMP[7], "over-scale clamps to the full block");
    }

    #[test]
    fn the_rolling_reference_scales_a_softer_voice_below_the_burst() {
        let mut meter = DictateMeter::new(-50.0);
        for _ in 0..40 {
            meter.push(-18.0);
        }
        let burst = meter.current();
        for _ in 0..8 {
            meter.push(-34.0);
        }
        let softer = meter.current();
        assert!(
            softer > 0.1 && softer < burst,
            "after a loud burst the reference holds, so a softer voice reads mid-scale \
             instead of pegging: softer {softer:.2}, burst {burst:.2}"
        );
    }

    #[test]
    fn the_meter_holds_a_fixed_window_and_drops_the_oldest_frame() {
        let mut meter = DictateMeter::new(-50.0);
        #[allow(clippy::cast_precision_loss)]
        for i in 0..(METER_WIDTH + 4) {
            meter.push(-50.0 + i as f32);
        }
        assert_eq!(meter.window().len(), METER_WIDTH, "the window holds its fixed width");
        assert!(
            meter.window().iter().all(|frac| frac.is_finite()),
            "every frame is a normalized fraction"
        );
        meter.push(-18.0);
        assert_eq!(meter.window().len(), METER_WIDTH, "one more push evicts, never grows");
    }

    #[test]
    fn the_meter_curve_pins_the_approved_constants() {
        let mut meter = DictateMeter::new(-50.0);
        meter.push(-20.0);
        assert!(
            (meter.env_db() + 32.0).abs() < 1e-3,
            "one attack step at 0.6 from -50 lands at -32, got {}",
            meter.env_db()
        );
        assert!(
            (meter.current() - 0.9392).abs() < 1e-3,
            "the first frame scales -32 against the decayed -30.7 reference under the 0.9 \
             gamma, got {}",
            meter.current()
        );
        meter.push(-20.0);
        assert!((meter.env_db() + 24.8).abs() < 1e-3, "the second attack step lands at -24.8");
        meter.push(-20.0);
        assert!((meter.env_db() + 21.92).abs() < 1e-3, "the third attack step lands at -21.92");
        assert!(
            (meter.current() - 1.0).abs() < 1e-3,
            "once the reference meets the envelope the meter reads full scale, got {}",
            meter.current()
        );
    }

    #[test]
    fn a_truncated_meter_keeps_the_newest_frames() {
        let mut app = App::test_default();
        let key = app.active_session_key.clone().expect("test_default has an active bucket");
        let bucket = app.session_mut(&key).expect("bucket");
        let mut indicator = DictateIndicator::recording(-50.0, 1);
        for _ in 0..10 {
            indicator.push_level(-6.0);
        }
        for _ in 0..20 {
            indicator.push_level(-52.0);
        }
        bucket.dictate = Some(indicator);

        // At 35 cols the meter shrinks to five cells; the newest five
        // frames are silence, so those cells are the floor glyph - the
        // loud frames have already scrolled off the left.
        let line = dictate_row_content(&app, 35);
        let cells: Vec<&str> = line.spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(
            &cells[6..11],
            &["\u{2581}", "\u{2581}", "\u{2581}", "\u{2581}", "\u{2581}"],
            "a narrow meter renders the newest frames, so the right edge is now"
        );
    }

    #[test]
    fn a_take_stamps_its_start_and_freezes_the_duration_at_handoff() {
        let indicator = DictateIndicator::recording(-50.0, 1);
        assert!(indicator.started.elapsed().as_millis() < 100, "the timer stamps at construction");

        let mut indicator = DictateIndicator::recording(-50.0, 1);
        indicator.begin_transcribing();
        let frozen = indicator.recording_duration.expect("the handoff freezes the duration");
        assert!(
            frozen <= indicator.started.elapsed(),
            "the frozen figure is the recording's own elapsed time"
        );
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(indicator.take_elapsed(), frozen, "transcription holds the take length still");
    }

    #[test]
    fn the_meter_freezes_at_its_last_recording_frame_on_handoff() {
        let mut indicator = DictateIndicator::recording(-50.0, 1);
        indicator.push_level(-18.0);
        indicator.push_level(-6.0);
        indicator.begin_transcribing();
        let frozen_frame: Vec<f32> = indicator.meter.window().to_vec();

        indicator.push_level(-2.0);
        indicator.push_level(-3.0);
        assert_eq!(
            indicator.meter.window(),
            frozen_frame.as_slice(),
            "a level reading that races past the handoff must not move the frozen frame"
        );
    }

    #[test]
    fn the_status_row_exists_only_while_a_take_is_live() {
        let mut app = App::test_default();
        app.dictate_available = true;
        assert!(!dictate_row_visible(&app), "idle reserves nothing");

        let key = app.active_session_key.clone().expect("test_default has an active bucket");
        let bucket = app.session_mut(&key).expect("bucket");
        bucket.dictate = Some(DictateIndicator::recording(-50.0, 1));
        assert!(dictate_row_visible(&app), "the box grows at record start");

        let bucket = app.session_mut(&key).expect("bucket");
        bucket.dictate = None;
        assert!(!dictate_row_visible(&app), "a resolved take shrinks the box again");
    }

    #[test]
    fn transcribing_hides_the_row_until_three_seconds_then_brings_it_back() {
        let mut app = App::test_default();
        let key = app.active_session_key.clone().expect("test_default has an active bucket");
        {
            let bucket = app.session_mut(&key).expect("bucket");
            let mut indicator = DictateIndicator::recording(-50.0, 1);
            indicator.begin_transcribing();
            bucket.dictate = Some(indicator);
        }
        assert!(
            !dictate_row_visible(&app),
            "a warm take never flashes the row back after the collapse"
        );

        let bucket = app.session_mut(&key).expect("bucket");
        let indicator = bucket.dictate.as_mut().expect("a take is in flight");
        indicator.transcribing_since = Some(
            Instant::now()
                .checked_sub(Duration::from_millis(3001))
                .expect("a 3 s backdate is safe"),
        );
        assert!(
            dictate_row_visible(&app),
            "only a cold start past the silence threshold redraws the row"
        );
    }

    #[test]
    fn a_stamped_notice_wins_the_row_slot() {
        let mut app = App::test_default();
        let key = app.active_session_key.clone().expect("test_default has an active bucket");
        let bucket = app.session_mut(&key).expect("bucket");
        bucket.dictate = Some(DictateIndicator::recording(-50.0, 1));
        bucket.set_dictate_notice(DictateNotice {
            severity: NoticeSeverity::Dim,
            text: "nothing above -50 dBFS in 4s \u{b7} try again".to_owned(),
        });

        assert!(dictate_row_visible(&app), "the notice keeps its row");
        let line = dictate_row_content(&app, 74);
        let text: String = line.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(
            text.contains("try again") && !text.contains("listening"),
            "the notice renders in the slot and the status row stands down: {text}"
        );
    }

    #[test]
    fn border_targets_follow_the_take_state() {
        let mut bucket = UiSession {
            dictate: Some(DictateIndicator::recording(-50.0, 1)),
            ..UiSession::default()
        };
        let quiet = border_target(&bucket);
        bucket.dictate.as_mut().expect("live").push_level(-6.0);
        let loud = border_target(&bucket);
        assert!(
            colour_distance(loud, HOT) < colour_distance(quiet, HOT),
            "a louder frame rides the border toward the hot tint"
        );

        bucket.dictate.as_mut().expect("bucket").begin_transcribing();
        assert!(
            same_colour(border_target(&bucket), BLUE),
            "transcription hands the border to blue"
        );

        bucket.dictate = None;
        assert!(
            same_colour(border_target(&bucket), ORANGE),
            "a resolved take targets the normal orange"
        );
    }

    #[test]
    fn the_afterglow_beats_green_then_eases_home() {
        let now = Instant::now();
        let beat = DictateBorder::Afterglow { started: now, rgb: BLUE, beat: true };
        let mid_beat = afterglow_colour(&beat, now + Duration::from_millis(200))
            .expect("inside the beat window the colour stands");
        assert!(
            colour_distance(mid_beat, GREEN) < colour_distance(BLUE, GREEN),
            "the beat eases the frozen colour toward green, got {mid_beat:?}"
        );
        let tail = afterglow_colour(&beat, now + Duration::from_millis(700))
            .expect("early in the tail the colour stands");
        assert!(
            colour_distance(tail, ORANGE) < colour_distance(GREEN, ORANGE),
            "past the beat the colour eases home to orange, got {tail:?}"
        );
        assert_eq!(
            afterglow_colour(&beat, now + Duration::from_millis(10_000)),
            None,
            "past the window the afterglow is gone with no render visit"
        );

        let cancelled = DictateBorder::Afterglow {
            started: now,
            rgb: BLUE,
            beat: false,
        };
        let easing_home = afterglow_colour(&cancelled, now + Duration::from_millis(50))
            .expect("a cancelled take eases home too");
        assert!(
            colour_distance(easing_home, ORANGE) < colour_distance(BLUE, ORANGE),
            "no beat: the frozen colour eases straight home, got {easing_home:?}"
        );
    }

    #[test]
    fn an_afterglow_self_expires_without_render_visits() {
        let mut app = App::test_default();
        let other = forge_workspace::SessionKey::from_session_id("other-project");
        app.sessions.insert(other.clone(), UiSession::new(other.clone()));
        {
            let bucket = app.sessions.get_mut(&other).expect("bucket");
            bucket.dictate = Some(DictateIndicator::recording(-50.0, 1));
            bucket.dictate_border = Some(DictateBorder::live(None, Instant::now()));
        }
        assert!(app.shows_activity(), "a background take is live work");

        // Its take resolves while another session holds the focus; the
        // reducer runs and leaves the afterglow behind.
        apply_session_update(
            &mut app,
            SessionUpdate::DictateEnded {
                key: other.clone(),
                generation: 1,
                outcome: DictateOutcome::Landed { text: "landed".to_owned(), truncated: false },
            },
        );
        assert!(app.shows_activity(), "the beat is still inside its window");

        let bucket = app.sessions.get_mut(&other).expect("bucket");
        let border = bucket.dictate_border.as_mut().expect("the afterglow persists on the bucket");
        let DictateBorder::Afterglow { started, .. } = border else {
            panic!("the resolved take left an afterglow, got {border:?}");
        };
        *started = Instant::now()
            .checked_sub(Duration::from_millis(5_000))
            .expect("a 5 s backdate is safe");
        assert!(
            !app.shows_activity(),
            "the afterglow self-expires with no border_color call in between"
        );
        assert!(
            !app.sessions.get(&other).expect("bucket").dictate.is_some(),
            "and the resolved take is gone from the bucket"
        );
    }

    #[test]
    fn the_border_state_drops_once_the_ease_settles() {
        let mut app = App::test_default();
        assert_eq!(border_color(&mut app, Instant::now()), None, "idle is untouched");
        let key = app.active_session_key.clone().expect("test_default has an active bucket");
        {
            let bucket = app.session_mut(&key).expect("bucket");
            bucket.dictate_border = Some(DictateBorder::Afterglow {
                started: Instant::now(),
                rgb: BLUE,
                beat: false,
            });
        }
        let now = Instant::now();
        assert!(
            border_color(&mut app, now).is_some(),
            "a settling afterglow still draws its eased colour"
        );
        let mut dropped = false;
        for step in 1..400 {
            if border_color(&mut app, now + Duration::from_millis(step * 50)).is_none() {
                dropped = true;
                break;
            }
        }
        assert!(dropped, "once back at the composer's orange the state goes away entirely");
        let bucket = app.session_mut(&key).expect("bucket");
        assert!(bucket.dictate_border.is_none(), "the bucket holds no border afterglow");
    }

    #[test]
    fn the_border_eases_toward_its_target_rather_than_jumping() {
        let mut app = App::test_default();
        let key = app.active_session_key.clone().expect("test_default has an active bucket");
        {
            let bucket = app.session_mut(&key).expect("bucket");
            let mut indicator = DictateIndicator::recording(-50.0, 1);
            indicator.begin_transcribing();
            bucket.dictate = Some(indicator);
            bucket.dictate_border = Some(DictateBorder::live(None, Instant::now()));
        }
        let now = Instant::now();
        let first = border_color(&mut app, now).expect("mid-ease colour");
        assert!(
            rgb_distance(first, rgbf(BLUE)) > 1.0,
            "one render step leaves the border short of the target, got {first:?}"
        );
        let mut last = first;
        for _ in 0..400 {
            let Some(colour) = border_color(&mut app, now + Duration::from_millis(50)) else {
                break;
            };
            last = colour;
        }
        assert!(
            rgb_distance(last, rgbf(BLUE)) <= 1.0,
            "the ease converges on the handoff blue, got {last:?}"
        );
        {
            let bucket = app.session_mut(&key).expect("bucket");
            assert!(
                bucket.dictate_border.is_some(),
                "a live take keeps its border state however settled the colour"
            );
        }
    }

    #[test]
    fn the_db_readout_refreshes_at_five_hertz() {
        let mut indicator = DictateIndicator::recording(-50.0, 1);
        for _ in 0..40 {
            indicator.push_level(-18.0);
        }
        let t0 = Instant::now();
        let (held, _) = indicator.db_readout(t0);
        for _ in 0..20 {
            indicator.push_level(-2.0);
        }
        let (still, _) = indicator.db_readout(t0 + Duration::from_millis(100));
        assert!((held - still).abs() < f32::EPSILON, "inside the 200 ms window the figure holds");
        let (fresh, _) = indicator.db_readout(t0 + Duration::from_millis(250));
        assert!(fresh > held, "past the window the figure refreshes toward the louder feed");
    }

    #[test]
    fn the_db_readout_is_a_recording_only_surface() {
        let mut app = App::test_default();
        assert_eq!(active_db_readout(&mut app, Instant::now()), None, "idle has no readout");
        let key = app.active_session_key.clone().expect("test_default has an active bucket");
        let bucket = app.session_mut(&key).expect("bucket");
        let mut indicator = DictateIndicator::recording(-50.0, 1);
        indicator.begin_transcribing();
        bucket.dictate = Some(indicator);
        assert_eq!(
            active_db_readout(&mut app, Instant::now()),
            None,
            "transcription returns the normal blinking cursor"
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
