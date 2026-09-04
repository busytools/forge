//! Preflight - the first of the launchpad's two views.
//!
//! Two sibling sections, `Accounts` and `Dictation`, each row carrying
//! its own state. Shown once per forge run, on every route; nothing
//! proceeds until every account has settled and every configured model
//! is loaded, so neither the project picker nor a chat session can be
//! reached mid-load.
//!
//! **Preflight completes when every account settles, not only when
//! every account is `Ready`.** A bailed account rides along as
//! degraded: its row names the failure, the assignment plan excludes
//! it, and the pollers keep re-probing it, so holding the screen waits
//! on nothing that could change it.
//!
//! Repairing the auth on a keychain account needs no restart; anything
//! that edits forge.toml - the account's env, or dropping the account -
//! does, because config is read once at boot and the pollers keep
//! probing what they loaded. What needs no restart is picked up in
//! place - on the 30 s recovery poll for a keychain account, on the
//! 60 s usage poll for a base-url or token-mode one, which the
//! recovery poll skips because it gates on `claude auth status`. The
//! screen offers the repairs in that order for that reason.
//!
//! Geometry matches [`super::launchpad`]: same wordmark, same
//! `PICKER_WIDTH` panel, so handing over to the project picker is a
//! content swap rather than a resize. The chat route redraws anyway, so
//! only the picker handover is a geometry claim.

use forge_workspace::{
    AccountAuth, AccountLoadingRow, DictateBind, DictateFailure, DictateModel, DictateModelState,
    DictateSnapshot, LoadingState, UsageFetchStatus,
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
/// on the one screen that gates forge starting, loading and failed
/// must not differ only by glyph. The project row's own account chip
/// uses the same red for the same state.
pub(super) fn account_glyph(state: LoadingState) -> (&'static str, Color) {
    match state {
        LoadingState::Loading | LoadingState::Refreshing => ("\u{25cb}", Color::Yellow),
        LoadingState::Ready => ("\u{25cf}", Color::Green),
        LoadingState::Bailed => ("\u{26a0}", theme::STATUS_ERROR),
    }
}

/// `true` when every account has settled into a terminal state -
/// `Ready` or `Bailed`. A bailed account no longer holds boot: the
/// assignment plan already excludes it and the pollers keep re-probing
/// it, so the screen would be waiting on nothing that could change it.
/// The preflight handover and the boot-spawn release in `app::connect`
/// share this one condition.
pub fn accounts_settled(app: &App) -> bool {
    app.workspace.as_ref().is_some_and(|ws| {
        let rows = ws.account_loading_snapshot();
        !rows.is_empty()
            && rows
                .iter()
                .all(|row| matches!(row.state, LoadingState::Ready | LoadingState::Bailed))
    })
}

/// `true` once preflight has nothing left to wait for.
pub fn is_complete(app: &App) -> bool {
    accounts_settled(app)
        && app.workspace.as_ref().is_some_and(|ws| ws.dictate_snapshot().is_ready())
}

/// Render the preflight screen over the whole frame.
pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    app.cached_frame_area = area;

    let panel_width = PICKER_WIDTH.min(area.width.saturating_sub(8));
    let body = panel_lines(app, panel_width);
    let cancelled = app
        .workspace
        .as_ref()
        .is_some_and(|ws| ws.dictate_snapshot().failure.is_some_and(|f| f.is_cancelled()));

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
    let painted = render_panel(frame, panel, body);

    // Set from what actually reached the buffer, not from having run:
    // a panel too small to paint its body would otherwise let forge quit
    // having said nothing about what the cancelled transfer kept.
    if cancelled && painted {
        app.preflight_cancel_drawn = true;
    }

    let footer = footer_hint(app);
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(footer, Style::default().fg(theme::DIM))])),
        Rect { x: area.x, y: area.y + available, width: area.width, height: footer_height },
    );
}

/// Paint the framing rules and whatever fits between them. Reports
/// whether the whole body reached the buffer, which is what
/// [`crate::app::preflight::quit_after_cancel`] waits on.
fn render_panel(frame: &mut Frame, area: Rect, body: Vec<Line<'static>>) -> bool {
    if area.width == 0 || area.height < 2 {
        return false;
    }
    let dim = Style::default().fg(theme::DIM);
    let rule = || Line::from(Span::styled("\u{2500}".repeat(usize::from(area.width)), dim));
    let inner = area.height - 2;
    frame.render_widget(
        Paragraph::new(rule()),
        Rect { x: area.x, y: area.y, width: area.width, height: 1 },
    );
    let complete = body.len() <= usize::from(inner);
    if inner > 0 {
        frame.render_widget(
            Paragraph::new(keep_the_tail(body, inner)),
            Rect { x: area.x, y: area.y + 1, width: area.width, height: inner },
        );
    }
    frame.render_widget(
        Paragraph::new(rule()),
        Rect { x: area.x, y: area.y + 1 + inner, width: area.width, height: 1 },
    );
    complete
}

/// Drop from the TOP when the body overflows, and say how much went.
///
/// Everything this screen cannot afford to lose - the failure detail and
/// the exits it names - is appended last, so a paragraph that simply
/// clips takes exactly the wrong end. At 100x24 the bailed screen is
/// already taller than the terminal.
fn keep_the_tail(mut body: Vec<Line<'static>>, height: u16) -> Vec<Line<'static>> {
    let height = usize::from(height);
    if height == 0 || body.len() <= height {
        return body;
    }
    // One row of the budget goes to saying what was dropped, so nothing
    // vanishes without a mark.
    let dropped = body.len() - height + 1;
    body.drain(..dropped);
    body.insert(
        0,
        Line::from(Span::styled(
            format!("  \u{2026} {dropped} more above"),
            Style::default().fg(theme::DIM).add_modifier(Modifier::DIM),
        )),
    );
    body
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
        if let Some(bind_warning) = dictate_bind_warning(app, width) {
            lines.push(bind_warning);
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
        LoadingState::Bailed => {
            // The state column carries the classified failure: an auth
            // problem, a rate limit, and an endpoint that is down or
            // answering badly are three different repairs.
            let state = match row.last_error {
                Some(UsageFetchStatus::NetworkFailed) => "unreachable",
                Some(UsageFetchStatus::Other) => "fetch error",
                Some(UsageFetchStatus::RateLimited) => "rate limited",
                _ => "auth failed",
            };
            (
                state,
                Style::default().fg(theme::STATUS_ERROR),
                Style::default().add_modifier(Modifier::BOLD),
            )
        }
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
    // A stopped preflight has nothing still turning, and both models run
    // at once now, so the one the failure is not about can be left
    // mid-transfer with a spinner going under a screen that says forge
    // is quitting. The state and its bar stay as they were, because they
    // are true and say how much of that model landed; only the animation
    // stops. `Pending` already reads differently under a failure for the
    // same reason.
    let spinner = if snapshot.failure.is_some() {
        "\u{25cb}".to_owned()
    } else {
        app.active_spinner_glyph().to_string()
    };
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
        DictateModelState::Failed(failure) => (
            "\u{26a0}".to_owned(),
            theme::STATUS_ERROR,
            failure_label(failure).to_owned(),
            Style::default().fg(theme::STATUS_ERROR),
        ),
    };
    let name_style = if matches!(model.state, DictateModelState::Failed(_)) {
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
    if let DictateModelState::Failed(DictateFailure::Cancelled { kept, total }) = &model.state {
        rows.push(bar_row(*kept, *total));
    }
    rows
}

fn failure_label(failure: &DictateFailure) -> &'static str {
    match failure {
        DictateFailure::HashMismatch { .. } => "bad hash",
        DictateFailure::Cancelled { .. } => "cancelled",
        DictateFailure::Other { .. } => "failed",
    }
}

/// The account that failed, and both ways past it.
///
/// Both, in that order, because they are not equivalent: a repair that
/// does not touch forge.toml is picked up in place, while dropping the
/// account or editing its env edits config, which is read once at boot
/// and so needs a restart. Naming only one would leave a reader who
/// cannot take that route with nowhere to go.
///
/// The head and the repair line key on the failure class: an auth
/// problem, a rate limit, and an endpoint that is down or answering
/// badly are three different repairs. The auth branch's retry line
/// states no interval on purpose, and keys on the account class: a
/// keychain repair is picked up in place by the recovery poll, while
/// a base-url or token repair is an env edit, and no single interval
/// is true of the polls that watch the classes.
fn bail_detail(app: &App, row: &AccountLoadingRow, width: u16) -> Vec<Line<'static>> {
    let error = Style::default().fg(theme::STATUS_ERROR);
    let head = Style::default().add_modifier(Modifier::BOLD);
    let config = app
        .workspace
        .as_ref()
        .map_or_else(|| "forge.toml".to_owned(), |ws| home_relative(&ws.config_path()));
    let endpoint_failing =
        matches!(row.last_error, Some(UsageFetchStatus::NetworkFailed | UsageFetchStatus::Other));
    let rate_limited = row.last_error == Some(UsageFetchStatus::RateLimited);

    let mut lines = if endpoint_failing {
        let head_line = if row.last_error == Some(UsageFetchStatus::NetworkFailed) {
            format!(
                "{} cannot be reached. forge starts without it and keeps retrying.",
                row.display_name
            )
        } else {
            // `Other` covers an endpoint that answered badly and a probe
            // that could not run at all, so the head claims neither.
            format!(
                "{} keeps failing its probe. forge starts without it and keeps retrying.",
                row.display_name
            )
        };
        wrapped(2, &head_line, error, width)
    } else if rate_limited {
        wrapped(
            2,
            &format!("{} is rate limited. forge starts without it.", row.display_name),
            error,
            width,
        )
    } else {
        // Both env-credential repairs are boot-frozen, so only a
        // keychain repair recovers in place.
        let tail = match row.auth {
            AccountAuth::Keychain => "fix the auth and it recovers in place",
            AccountAuth::BaseUrl => "fix the auth and restart forge to pick the new token up",
            AccountAuth::Token => "fix the auth and restart forge to pick the re-mint up",
        };
        wrapped(
            2,
            &format!(
                "{} will not start a session. forge starts without it; {}.",
                row.display_name, tail
            ),
            error,
            width,
        )
    };
    lines.push(Line::default());
    if endpoint_failing {
        lines.push(text_row(2, "Check the endpoint", head, width));
        match row.auth {
            AccountAuth::Keychain | AccountAuth::Token => {
                let line = if row.last_error == Some(UsageFetchStatus::NetworkFailed) {
                    "the probe could not run or reach the Anthropic API"
                } else {
                    "the Anthropic API keeps failing its probe"
                };
                lines.push(text_row(4, line, Style::default(), width));
            }
            AccountAuth::BaseUrl => {
                lines.push(text_row(
                    4,
                    "ANTHROPIC_BASE_URL in [accounts.env], or the endpoint itself",
                    Style::default(),
                    width,
                ));
            }
        }
        // The account's env is read from forge.toml once at boot, so an
        // edited base url changes nothing for the pollers until restart.
        // Wrapped, never truncated: it is guidance the reader acts on.
        lines.extend(wrapped(
            4,
            "editing forge.toml needs a restart; fixing the endpoint does not",
            dim(),
            width,
        ));
    } else if rate_limited {
        lines.push(text_row(4, "Waiting clears it - the pollers keep retrying", dim(), width));
    } else {
        lines.push(text_row(2, "Fix the auth", head, width));
        // The only thing that differs by account class. A base-url account
        // has no keychain entry for `/login` to write - its credential is
        // the token beside it in `[accounts.env]`.
        match row.auth {
            AccountAuth::Keychain => {
                lines.extend(command_rows(
                    4,
                    &format!("CLAUDE_CONFIG_DIR={} claude", home_relative(&row.config_dir)),
                    width,
                ));
                lines.push(text_row(4, "/login", Style::default(), width));
            }
            AccountAuth::BaseUrl => {
                lines.push(text_row(
                    4,
                    "ANTHROPIC_AUTH_TOKEN in [accounts.env]",
                    Style::default(),
                    width,
                ));
            }
            AccountAuth::Token => {
                lines.push(text_row(
                    4,
                    "CLAUDE_CODE_OAUTH_TOKEN in [accounts.env]",
                    Style::default(),
                    width,
                ));
                lines.extend(command_rows(4, "claude setup-token", width));
            }
        }
        // Without this a reader who fixes their auth has no way of
        // knowing whether to restart. Keychain: the recovery poll picks
        // the repair up in place. Base-url: the repaired token lives in
        // [accounts.env], which is read once at boot, so the retry
        // cannot see it until forge restarts.
        match row.auth {
            AccountAuth::Keychain => {
                lines.push(text_row(
                    4,
                    "forge retries on its own - no restart needed",
                    dim(),
                    width,
                ));
            }
            AccountAuth::BaseUrl | AccountAuth::Token => {
                lines.push(text_row(4, "editing [accounts.env] needs a restart", dim(), width));
            }
        }
    }
    lines.push(Line::default());
    lines.push(text_row(2, "Or drop the account", head, width));
    lines.push(text_row(4, "delete its [[accounts]] block from", Style::default(), width));
    lines.extend(command_rows(4, &config, width));
    lines
}

/// What went wrong with a model, in the crate's own words plus what to
/// do about it.
fn dictate_bind_warning(app: &App, width: u16) -> Option<Line<'static>> {
    let bound = app.workspace.as_ref().is_some_and(|ws| ws.dictate_bind() != DictateBind::Off);
    let delivered = crate::app::keyboard_enhancement_supported();
    if !bound || delivered != Some(false) {
        return None;
    }
    Some(text_row(
        2,
        "this terminal dropped the keyboard flags \u{b7} the push-to-talk key will not arrive",
        Style::default().fg(theme::STATUS_WARNING),
        width,
    ))
}

fn dictate_detail(failure: &DictateFailure, width: u16) -> Vec<Line<'static>> {
    let error = Style::default().fg(theme::STATUS_ERROR);
    let head = Style::default().add_modifier(Modifier::BOLD);
    match failure {
        DictateFailure::HashMismatch { path, expected, actual, size } => {
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
                &format!(
                    "It is the right length, so this is corruption and not a half-finished \
                     download. forge reports it rather than deleting it: throwing away a {} \
                     file you put there is not forge's call.",
                    bytes(*size)
                ),
                Style::default(),
                width,
            ));
            lines.push(Line::default());
            lines.push(text_row(2, "Delete it and forge fetches it again", head, width));
            lines.extend(command_rows(4, &format!("rm {}", home_relative(path)), width));
            lines
        }
        DictateFailure::Cancelled { kept, .. } => {
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
    if dictate.failure.as_ref().is_some_and(DictateFailure::is_cancelled) {
        return " quitting\u{2026}".to_owned();
    }
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

/// A bar with no known total draws empty rather than full: it is
/// reached only when nothing was in flight, and a full bar would read as
/// a transfer that finished.
fn bar_row(downloaded: u64, total: u64) -> Line<'static> {
    let bar = u64::try_from(BAR_WIDTH).unwrap_or(u64::MAX);
    let (filled, label) = if total == 0 {
        (0, bytes(downloaded))
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
    let mut lines = Vec::new();
    let mut rest = text;
    let mut current = indent;
    loop {
        let (lead, room) = command_row_budget(current, width);
        if rest.chars().count() <= room {
            break;
        }
        // Byte offset of the character after the last one that fits.
        // `rest` is longer than `room` characters and `room` is at least
        // one, so this always lands past the first character - which is
        // what makes every branch below advance. Counted in characters
        // and cut on the boundary that count resolves to: mixing the two
        // agrees on ASCII, then cuts at zero on the first accented path
        // and spins here forever.
        let hard = rest.char_indices().nth(room).map_or(rest.len(), |(offset, _)| offset);
        // Break after the last separator that fits, so a continuation
        // line starts on a path segment rather than mid-word.
        let cut = rest[..hard].rfind('/').map_or(hard, |offset| offset + 1);
        lines.push(command_row(lead, &rest[..cut]));
        rest = &rest[cut..];
        // Continuation sits deeper, so a wrapped command still reads as
        // one command rather than as two.
        current = indent + 3;
    }
    if !rest.is_empty() || lines.is_empty() {
        lines.push(command_row(command_row_budget(current, width).0, rest));
    }
    lines
}

/// Indent and character budget for one command row. A panel too narrow
/// to afford the indent gives it up rather than the text: alignment is
/// worth less than being able to read the command.
fn command_row_budget(indent: usize, width: u16) -> (usize, usize) {
    let width = usize::from(width);
    let lead = indent.min(width.saturating_sub(1));
    (lead, width.saturating_sub(lead + 2).max(1))
}

/// One row of a command, laid out but never shortened. [`text_row`]
/// truncates, and a truncated command is one the reader cannot run.
fn command_row(indent: usize, text: &str) -> Line<'static> {
    Line::from(vec![Span::raw(" ".repeat(indent)), Span::raw(text.to_owned())])
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
