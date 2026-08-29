//! Preflight - the first of the launchpad's two views.
//!
//! Two sibling sections, `Accounts` and `Dictation`, each row carrying
//! its own state. Shown once per forge run; nothing proceeds until
//! every account is `Ready` and every configured model is loaded, so
//! the projects pane can no longer be reached mid-load.
//!
//! **Preflight completes only on `Ready`, never on `Bailed`.** forge
//! will not start while an account in `forge.toml` cannot, which makes
//! a config edit the only way past this screen - so the failure states
//! name both exits rather than leaving the reader stuck on a screen
//! with nothing to press.
//!
//! Geometry matches [`super::launchpad`]: same wordmark, same
//! `PICKER_WIDTH` panel, so handing over to the projects view is a
//! content swap rather than a resize.

use forge_workspace::{
    AccountLoadingRow, DictateFailure, DictateModel, DictateModelState, DictateSnapshot,
    LoadingState,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::launchpad::{PICKER_WIDTH, identity_lines};
use super::theme;
use crate::app::App;

/// Right-aligned state column. `downloading` is the longest word that
/// lands in it.
const STATE_WIDTH: usize = 12;

/// Name column: the panel less the 2-cell indent, the glyph and its
/// separating space, the state column, and the 2-cell right margin.
const NAME_WIDTH: usize = PICKER_WIDTH as usize - 2 - 1 - 1 - STATE_WIDTH - 2;

/// Cells in a progress bar.
const BAR_WIDTH: usize = 26;

/// Blank rows kept below the panel so the block does not sit flush
/// against the footer.
const PANEL_BOTTOM_MARGIN: u16 = 1;

/// Glyph and colour for one account's loading state. Shared with the
/// launchpad's own per-account row so the two surfaces cannot drift
/// apart on what green means.
///
/// `Bailed` is [`theme::STATUS_ERROR`] rather than the warning yellow:
/// on the one screen that can stop forge starting, loading and failed
/// must not differ only by glyph. It also settles a disagreement with
/// the project row's own account chip, which was already red.
pub(super) fn account_glyph(state: LoadingState) -> (&'static str, Color) {
    match state {
        LoadingState::Loading | LoadingState::Refreshing => ("\u{25cb}", Color::Yellow),
        LoadingState::Ready => ("\u{25cf}", Color::Green),
        LoadingState::Bailed => ("\u{26a0}", theme::STATUS_ERROR),
    }
}

/// `true` when every account has authenticated. Bailed is terminal and
/// never satisfies it, which is what stops forge starting over an
/// account that cannot spawn a session.
pub fn accounts_ready(app: &App) -> bool {
    app.workspace.as_ref().is_some_and(|ws| {
        let rows = ws.account_loading_snapshot();
        !rows.is_empty() && rows.iter().all(|row| row.state == LoadingState::Ready)
    })
}

/// `true` once preflight has nothing left to wait for.
pub fn is_complete(app: &App) -> bool {
    accounts_ready(app) && app.workspace.as_ref().is_some_and(|ws| ws.dictate_snapshot().is_ready())
}

/// Render the preflight screen over the whole frame.
pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    app.cached_frame_area = area;

    let panel_width = PICKER_WIDTH.min(area.width.saturating_sub(8));
    let body = panel_lines(app, panel_width);

    // Recorded here rather than at the key press, so forge only quits
    // once this frame has actually said what the cancelled transfer
    // kept and where it left it.
    if app
        .workspace
        .as_ref()
        .is_some_and(|ws| ws.dictate_snapshot().failure.is_some_and(|f| f.is_cancelled()))
    {
        app.preflight_cancel_drawn = true;
    }

    let footer_height: u16 = 1;
    let available = area.height.saturating_sub(footer_height);
    let identity = identity_lines(app, area.width);

    // Panel rows plus the two framing rules and the blank between the
    // identity block and the panel.
    let panel_height = u16::try_from(body.len()).unwrap_or(u16::MAX).saturating_add(2);
    let identity_height = u16::try_from(identity.len()).unwrap_or(u16::MAX).saturating_add(1);

    // The wordmark is decoration and the failure screens' exits are
    // not, so on a terminal too short for both the wordmark goes.
    let with_identity =
        identity_height.saturating_add(panel_height).saturating_add(PANEL_BOTTOM_MARGIN)
            <= available;
    let block_height = if with_identity { identity_height + panel_height } else { panel_height };
    let top = area.y + available.saturating_sub(block_height) / 2;

    let mut y = top;
    if with_identity {
        let height = u16::try_from(identity.len()).unwrap_or(u16::MAX);
        frame.render_widget(
            Paragraph::new(identity),
            Rect { x: area.x, y, width: area.width, height },
        );
        y += height + 1;
    }

    let panel = Rect {
        x: area.x + area.width.saturating_sub(panel_width) / 2,
        y,
        width: panel_width,
        height: panel_height.min(available.saturating_sub(y - area.y)),
    };
    render_panel(frame, panel, body);

    let footer = footer_hint(app);
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(footer, Style::default().fg(theme::DIM))])),
        Rect { x: area.x, y: area.y + available, width: area.width, height: footer_height },
    );
}

/// Paint the framing rules and whatever fits between them.
fn render_panel(frame: &mut Frame, area: Rect, body: Vec<Line<'static>>) {
    if area.width == 0 || area.height < 2 {
        return;
    }
    let dim = Style::default().fg(theme::DIM);
    let rule = || Line::from(Span::styled("\u{2500}".repeat(usize::from(area.width)), dim));
    let inner = area.height - 2;
    frame.render_widget(
        Paragraph::new(rule()),
        Rect { x: area.x, y: area.y, width: area.width, height: 1 },
    );
    if inner > 0 {
        frame.render_widget(
            Paragraph::new(body),
            Rect { x: area.x, y: area.y + 1, width: area.width, height: inner },
        );
    }
    frame.render_widget(
        Paragraph::new(rule()),
        Rect { x: area.x, y: area.y + 1 + inner, width: area.width, height: 1 },
    );
}

/// Everything between the two framing rules.
fn panel_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let mut lines = vec![heading_row("Accounts", width)];
    let accounts =
        app.workspace.as_ref().map(|ws| ws.account_loading_snapshot()).unwrap_or_default();
    for row in &accounts {
        lines.push(account_row(row, width));
    }

    let dictate = app.workspace.as_ref().map(|ws| ws.dictate_snapshot()).unwrap_or_default();
    if !dictate.models.is_empty() {
        lines.push(Line::default());
        lines.push(heading_row("Dictation", width));
        for model in &dictate.models {
            lines.extend(model_rows(app, model, &dictate, width));
        }
    }

    if let Some(bailed) = accounts.iter().find(|row| row.state == LoadingState::Bailed) {
        lines.push(Line::default());
        lines.extend(bail_detail(app, bailed, width));
    } else if let Some(failure) = dictate.failure.as_ref() {
        lines.push(Line::default());
        lines.extend(dictate_detail(failure, width));
    } else if dictate.models.iter().any(is_first_run_transfer) {
        lines.push(Line::default());
        lines.extend(first_run_note(app, width));
    }
    lines
}

fn is_transferring(model: &DictateModel) -> bool {
    matches!(model.state, DictateModelState::Downloading { .. })
}

/// A transfer that started from nothing, as opposed to one picking up a
/// `.part`. Only the first kind is about to move three gigabytes, so
/// only it earns the note saying so.
fn is_first_run_transfer(model: &DictateModel) -> bool {
    matches!(model.state, DictateModelState::Downloading { resumed_from: None, .. })
}

fn account_row(row: &AccountLoadingRow, width: u16) -> Line<'static> {
    let (glyph, color) = account_glyph(row.state);
    let (state, state_style, name_style) = match row.state {
        LoadingState::Ready => ("ready", dim(), Style::default()),
        LoadingState::Loading | LoadingState::Refreshing => ("resolving", dim(), Style::default()),
        LoadingState::Bailed => (
            "auth failed",
            Style::default().fg(theme::STATUS_ERROR),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    };
    status_row(glyph, color, &row.display_name, name_style, state, state_style, width)
}

/// One model as its two or three rows: role and state, the file
/// underneath, and a bar while bytes are moving.
///
/// The file cannot sit beside the role - `transcribing model
/// (cohere-transcribe-03-2026-Q4_K_M)` is 53 cells against a 38-cell
/// name column - and widening the panel would make the handover to the
/// projects view a resize rather than a content swap.
fn model_rows(
    app: &App,
    model: &DictateModel,
    snapshot: &DictateSnapshot,
    width: u16,
) -> Vec<Line<'static>> {
    let spinner = app.active_spinner_glyph().to_string();
    let (glyph, color, state, state_style) = match &model.state {
        DictateModelState::Pending => {
            // Nothing else is going to start, so `queued` would be a
            // promise preflight is not keeping.
            let label = if snapshot.failure.is_some() { "not started" } else { "queued" };
            ("\u{25cb}".to_owned(), Color::Yellow, label.to_owned(), dimmer())
        }
        DictateModelState::Downloading { resumed_from, .. } => (
            spinner,
            theme::RUST_ORANGE,
            if resumed_from.is_some() { "resuming" } else { "downloading" }.to_owned(),
            dim(),
        ),
        DictateModelState::Verifying => {
            (spinner, theme::RUST_ORANGE, "verifying".to_owned(), dim())
        }
        DictateModelState::Loading => (spinner, theme::RUST_ORANGE, "loading".to_owned(), dim()),
        // Verified-on-disk and loaded-in-memory both read `ready`: the
        // rows go through `loading` together in between, so the word
        // never means two things at once on screen.
        DictateModelState::Fetched | DictateModelState::Ready => {
            ("\u{25cf}".to_owned(), Color::Green, "ready".to_owned(), dim())
        }
        DictateModelState::Failed => (
            "\u{26a0}".to_owned(),
            theme::STATUS_ERROR,
            failure_label(snapshot.failure.as_ref()).to_owned(),
            Style::default().fg(theme::STATUS_ERROR),
        ),
    };
    let name_style = if model.state == DictateModelState::Failed {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    let mut rows = vec![
        status_row(&glyph, color, model.role.label(), name_style, &state, state_style, width),
        file_row(&model.file, width),
    ];
    if let DictateModelState::Downloading { downloaded, total, resumed_from } = &model.state {
        rows.push(bar_row(*downloaded, *total));
        if let Some(resumed) = resumed_from {
            rows.push(text_row(
                4,
                &format!("resumed from {} found in .part", bytes(*resumed)),
                dim(),
                width,
            ));
        }
    }
    // A cancelled transfer keeps its bar, so the screen can say how much
    // of the 3 GB is already on disk rather than only that it stopped.
    if model.state == DictateModelState::Failed
        && let Some(DictateFailure::Cancelled { kept }) = snapshot.failure.as_ref()
    {
        rows.push(bar_row(*kept, 0));
    }
    rows
}

fn failure_label(failure: Option<&DictateFailure>) -> &'static str {
    match failure {
        Some(DictateFailure::HashMismatch { .. }) => "bad hash",
        Some(DictateFailure::Cancelled { .. }) => "cancelled",
        _ => "failed",
    }
}

/// The account that will not authenticate, and both ways past it.
///
/// A config edit is the only escape from this screen, so naming just
/// one exit would leave a reader who cannot fix the auth with nowhere
/// to go.
fn bail_detail(app: &App, row: &AccountLoadingRow, width: u16) -> Vec<Line<'static>> {
    let error = Style::default().fg(theme::STATUS_ERROR);
    let head = Style::default().add_modifier(Modifier::BOLD);
    let config = app
        .workspace
        .as_ref()
        .map_or_else(|| "forge.toml".to_owned(), |ws| home_relative(&ws.config_path()));

    let mut lines = wrapped(
        2,
        &format!(
            "{} will not start a session, and forge will not start while an account in \
             forge.toml cannot.",
            row.display_name
        ),
        error,
        width,
    );
    lines.push(Line::default());
    lines.push(text_row(2, "Fix the auth", head, width));
    lines.extend(command_rows(
        4,
        &format!("CLAUDE_CONFIG_DIR={} claude", home_relative(&row.config_dir)),
        width,
    ));
    lines.push(text_row(4, "/login", Style::default(), width));
    lines.push(Line::default());
    lines.push(text_row(2, "Or drop the account", head, width));
    lines.push(text_row(4, "delete its [[accounts]] block from", Style::default(), width));
    lines.extend(command_rows(4, &config, width));
    lines
}

/// What went wrong with a model, in the crate's own words plus what to
/// do about it.
fn dictate_detail(failure: &DictateFailure, width: u16) -> Vec<Line<'static>> {
    let error = Style::default().fg(theme::STATUS_ERROR);
    let head = Style::default().add_modifier(Modifier::BOLD);
    match failure {
        DictateFailure::HashMismatch { path, expected, actual } => {
            let name = file_name(path);
            let mut lines = vec![
                text_row(2, &format!("{name} hashes to"), error, width),
                text_row(4, &short_hash(actual), head, width),
                text_row(2, "expected", error, width),
                text_row(4, &short_hash(expected), head, width),
                Line::default(),
            ];
            lines.extend(wrapped(
                2,
                "It is the right length, so this is corruption and not a half-finished \
                 download. forge reports it rather than deleting it: throwing away a file you \
                 put there is not forge's call.",
                Style::default(),
                width,
            ));
            lines.push(Line::default());
            lines.push(text_row(2, "Delete it and forge fetches it again", head, width));
            lines.extend(command_rows(4, &format!("rm {}", home_relative(path)), width));
            lines
        }
        DictateFailure::Cancelled { kept } => {
            let mut lines = wrapped(
                2,
                &format!(
                    "Nothing was thrown away. {} is on disk as a .part file and the next run \
                     resumes from there.",
                    bytes(*kept)
                ),
                Style::default(),
                width,
            );
            lines.push(Line::default());
            lines.push(text_row(2, "forge is quitting.", head, width));
            lines
        }
        DictateFailure::Other { message } => wrapped(2, message, error, width),
    }
}

/// Said while the first run is fetching, because 3 GB with no
/// explanation reads as forge having hung.
fn first_run_note(app: &App, width: u16) -> Vec<Line<'static>> {
    let dir = app
        .workspace
        .as_ref()
        .and_then(|ws| ws.dictate_models_dir())
        .map_or_else(|| "the models directory".to_owned(), |dir| home_relative(&dir));
    wrapped(
        2,
        &format!("First run fetches 3.07 GB once. Quitting keeps what has landed, in {dir}."),
        Style::default(),
        width,
    )
}

fn footer_hint(app: &App) -> String {
    let dictate = app.workspace.as_ref().map(|ws| ws.dictate_snapshot()).unwrap_or_default();
    // Escape only means something while bytes are moving: there is
    // nothing to cancel once every transfer has finished, and a hint
    // for a key that does nothing is worse than no hint.
    if dictate.failure.is_none() && dictate.models.iter().any(is_transferring) {
        let esc = if dictate.models.iter().any(is_first_run_transfer) {
            "esc  cancel and quit"
        } else {
            "esc  cancel"
        };
        return format!(" {esc}     ctrl+q  quit");
    }
    " ctrl+q  quit".to_owned()
}

fn dim() -> Style {
    Style::default().fg(theme::DIM)
}

/// A step quieter than [`dim`], for a row nothing has started on.
fn dimmer() -> Style {
    Style::default().fg(theme::DIM).add_modifier(Modifier::DIM)
}

/// `  <glyph> <name...> <state>  ` - the one row shape both sections
/// use, which is what makes them read as siblings.
fn status_row(
    glyph: &str,
    glyph_color: Color,
    name: &str,
    name_style: Style,
    state: &str,
    state_style: Style,
    width: u16,
) -> Line<'static> {
    let name_width = name_column(width);
    let label = super::launchpad::truncate_to(name, name_width);
    let pad = name_width.saturating_sub(label.chars().count());
    Line::from(vec![
        Span::raw("  "),
        Span::styled(glyph.to_owned(), Style::default().fg(glyph_color)),
        Span::raw(" "),
        Span::styled(label, name_style),
        Span::raw(" ".repeat(pad)),
        Span::styled(format!("{state:>STATE_WIDTH$}"), state_style),
    ])
}

/// The name column at `width`, which is [`NAME_WIDTH`] on any terminal
/// wide enough for the full panel.
fn name_column(width: u16) -> usize {
    usize::from(width).saturating_sub(2 + 1 + 1 + STATE_WIDTH + 2).min(NAME_WIDTH)
}

fn heading_row(text: &str, width: u16) -> Line<'static> {
    text_row(2, text, dim().add_modifier(Modifier::BOLD), width)
}

/// The file under its role, dim: subordinate detail somebody reads when
/// they care which file it is.
fn file_row(file: &str, width: u16) -> Line<'static> {
    let stem = file.strip_suffix(".gguf").unwrap_or(file);
    text_row(4, &format!("({stem})"), dimmer(), width)
}

/// `total` of zero draws a full bar: a cancelled transfer reports what
/// is on disk, not a fraction of a download that is no longer running.
fn bar_row(downloaded: u64, total: u64) -> Line<'static> {
    let bar = u64::try_from(BAR_WIDTH).unwrap_or(u64::MAX);
    let (filled, label) = if total == 0 {
        (BAR_WIDTH, bytes(downloaded))
    } else {
        let done = downloaded.min(total);
        // Rounded rather than truncated: 37.99% reading as 37 is off
        // by a percentage point for the whole last chunk of a transfer.
        let round = |scale: u64| (done.saturating_mul(scale) + total / 2) / total;
        let filled = usize::try_from(round(bar)).unwrap_or(BAR_WIDTH);
        let percent = round(100);
        (filled.min(BAR_WIDTH), format!("{percent:>3}%  {} / {}", bytes(done), bytes(total)))
    };
    Line::from(vec![
        Span::raw("    "),
        Span::styled("\u{2588}".repeat(filled), Style::default().fg(theme::RUST_ORANGE)),
        Span::styled("\u{2591}".repeat(BAR_WIDTH - filled), dimmer()),
        Span::styled(format!("  {label}"), dim()),
    ])
}

fn text_row(indent: usize, text: &str, style: Style, width: u16) -> Line<'static> {
    let budget = usize::from(width).saturating_sub(indent + 2);
    Line::from(vec![
        Span::raw(" ".repeat(indent)),
        Span::styled(super::launchpad::truncate_to(text, budget), style),
    ])
}

/// A command, wrapped rather than truncated, breaking after a `/` so
/// the split reads as a path rather than as damage.
///
/// Never elided: every one of these is on screen because the reader has
/// to run it, and half a path is worse than none.
fn command_rows(indent: usize, text: &str, width: u16) -> Vec<Line<'static>> {
    let budget = usize::from(width).saturating_sub(indent + 2).max(1);
    if text.chars().count() <= budget {
        return vec![text_row(indent, text, Style::default(), width)];
    }
    let mut lines = Vec::new();
    let mut rest = text;
    let mut current = indent;
    while rest.chars().count() > usize::from(width).saturating_sub(current + 2).max(1) {
        let room = usize::from(width).saturating_sub(current + 2).max(1);
        // Break after the last separator that fits, so a continuation
        // line starts on a path segment rather than mid-word.
        let hard = char_boundary_at_or_before(rest, room);
        let cut = rest[..hard].rfind('/').map_or(hard, |i| i + 1);
        let cut = if cut == 0 { hard } else { cut };
        lines.push(text_row(current, &rest[..cut], Style::default(), width));
        rest = &rest[cut..];
        // Continuation sits deeper, so a wrapped command still reads as
        // one command rather than as two.
        current = indent + 3;
    }
    if !rest.is_empty() {
        lines.push(text_row(current, rest, Style::default(), width));
    }
    lines
}

/// The largest char boundary at or before `at` bytes.
fn char_boundary_at_or_before(text: &str, at: usize) -> usize {
    let mut cut = at.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    cut
}

/// Prose across as many rows as it takes. Truncating a sentence that
/// names the way out of a screen forge will not leave is not an option.
fn wrapped(indent: usize, text: &str, style: Style, width: u16) -> Vec<Line<'static>> {
    let budget = usize::from(width).saturating_sub(indent + 2).max(1);
    let mut lines = Vec::new();
    let mut row = String::new();
    for word in text.split_whitespace() {
        let extra = usize::from(!row.is_empty());
        if row.chars().count() + extra + word.chars().count() > budget && !row.is_empty() {
            lines.push(text_row(indent, &row, style, width));
            row.clear();
        }
        if !row.is_empty() {
            row.push(' ');
        }
        row.push_str(word);
    }
    if !row.is_empty() {
        lines.push(text_row(indent, &row, style, width));
    }
    lines
}

/// `~/...` where the path is under the home directory. The reader is
/// about to leave forge and type this.
fn home_relative(path: &std::path::Path) -> String {
    dirs::home_dir()
        .and_then(|home| path.strip_prefix(home).ok())
        .map_or_else(|| path.display().to_string(), |rest| format!("~/{}", rest.display()))
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// Enough hex to tell two digests apart, because 64 characters do not
/// fit and nobody compares more than the ends anyway.
fn short_hash(digest: &str) -> String {
    digest.chars().take(12).collect()
}

fn bytes(count: u64) -> String {
    // Decimal MB and GB, matching what the model specs and every
    // download UI quote. Integer maths: hundredths of a GB, then a
    // decimal point put in by hand.
    if count >= 1_000_000_000 {
        let hundredths = (count + 5_000_000) / 10_000_000;
        return format!("{}.{:02} GB", hundredths / 100, hundredths % 100);
    }
    format!("{} MB", (count + 500_000) / 1_000_000)
}

#[cfg(test)]
mod tests;
