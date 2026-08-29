use super::*;
use forge_workspace::DictateRole;

/// Render one panel body at the shipped width and flatten it to plain
/// rows - the form the approved mock is drawn in.
fn rows(app: &App) -> Vec<String> {
    panel_lines(app, PICKER_WIDTH)
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
        .collect()
}

fn flatten(lines: &[Line<'static>]) -> Vec<String> {
    lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
}

fn row_containing<'a>(rows: &'a [String], needle: &str) -> &'a str {
    rows.iter()
        .find(|r| r.contains(needle))
        .unwrap_or_else(|| panic!("no row contains {needle:?}; got {rows:#?}"))
}

fn model(role: DictateRole, file: &str, state: DictateModelState) -> DictateModel {
    DictateModel { role, file: file.to_owned(), state }
}

fn account(name: &str, state: LoadingState, dir: &str) -> AccountLoadingRow {
    AccountLoadingRow {
        display_name: name.to_owned(),
        state,
        config_dir: std::path::PathBuf::from(dir),
    }
}

/// The two sections read as siblings: same indent, same right-aligned
/// state column. Asserted on the columns rather than on the words,
/// because "siblings" is a geometry claim - a heading that drifted two
/// cells would still contain the right text.
#[test]
fn the_two_sections_share_one_row_geometry() {
    let accounts_heading = flatten(&[heading_row("Accounts", PICKER_WIDTH)]).remove(0);
    let dictation_heading = flatten(&[heading_row("Dictation", PICKER_WIDTH)]).remove(0);
    let row =
        flatten(&[account_row(&account("Subspace", LoadingState::Ready, "/x"), PICKER_WIDTH)])
            .remove(0);

    assert_eq!(
        accounts_heading.find("Accounts"),
        dictation_heading.find("Dictation"),
        "the two section labels must start in the same column",
    );
    assert_eq!(
        accounts_heading.find("Accounts"),
        Some(2),
        "headings sit at the panel's 2-cell indent",
    );
    assert!(row.ends_with("       ready"), "the state column is right-aligned; got {row:?}");
    assert_eq!(
        row.chars().count(),
        usize::from(PICKER_WIDTH) - 2,
        "a row fills the panel less its 2-cell right margin; got {row:?}",
    );
}

/// The file cannot share the row with its role - the two together are
/// 53 cells against a 38-cell name column - so it goes on a dim line
/// beneath. Widening the panel instead would make the handover to the
/// projects view a resize rather than a content swap.
#[test]
fn a_model_reads_by_role_with_its_file_beneath() {
    let flat = flatten(&model_rows(
        &App::test_default(),
        &model(
            DictateRole::Transcribing,
            "cohere-transcribe-03-2026-Q4_K_M.gguf",
            DictateModelState::Ready,
        ),
        &DictateSnapshot::default(),
        PICKER_WIDTH,
    ));

    assert!(
        "transcribing model (cohere-transcribe-03-2026-Q4_K_M)".chars().count()
            > name_column(PICKER_WIDTH),
        "this test is only worth having while the inline form genuinely does not fit",
    );
    assert!(
        flat[0].contains("transcribing model") && flat[0].trim_end().ends_with("ready"),
        "the row reads by role and carries the state; got {:?}",
        flat[0],
    );
    assert!(
        !flat[0].contains("cohere"),
        "the file must not share the row - it does not fit; got {:?}",
        flat[0],
    );
    assert_eq!(
        flat[1].trim_end(),
        "    (cohere-transcribe-03-2026-Q4_K_M)",
        "the file sits on its own line under the role, without the extension",
    );
}

/// A bailed account stops forge starting and a config edit is the only
/// way past, so the screen has to name BOTH exits. Either one alone
/// strands a reader who cannot take that route.
#[test]
fn a_bailed_account_names_both_exits() {
    let text = flatten(&bail_detail(
        &App::test_default(),
        &account("Granite1", LoadingState::Bailed, "/home/x/.claude-granite1"),
        PICKER_WIDTH,
    ))
    .join("\n");

    assert!(
        text.contains("Granite1 will not start"),
        "the screen names the account that stopped it; got:\n{text}",
    );
    assert!(
        text.contains("Fix the auth") && text.contains("/login"),
        "exit one is fixing the auth, with the command; got:\n{text}",
    );
    assert!(
        text.contains("CLAUDE_CONFIG_DIR=/home/x/.claude-granite1"),
        "the /login line carries that account's own config dir; got:\n{text}",
    );
    assert!(
        text.contains("Or drop the account") && text.contains("[[accounts]]"),
        "exit two is removing the account from forge.toml; got:\n{text}",
    );
}

/// `Bailed` is red rather than the shipped warning yellow. On the one
/// screen that can stop forge starting, mid-flight and failed must not
/// differ only by glyph.
#[test]
fn a_bailed_account_is_red_not_yellow() {
    assert_eq!(account_glyph(LoadingState::Bailed).1, theme::STATUS_ERROR);
    assert_eq!(account_glyph(LoadingState::Loading).1, Color::Yellow);
    assert_eq!(account_glyph(LoadingState::Ready).1, Color::Green);
}

/// The Dictation section is absent entirely when dictation is off, and
/// preflight waits on the accounts alone.
#[test]
fn dictation_off_draws_no_section() {
    let rendered = rows(&App::test_default());
    assert!(
        rendered.iter().any(|r| r.contains("Accounts")),
        "the Accounts section is always drawn; got {rendered:#?}",
    );
    assert!(
        !rendered.iter().any(|r| r.contains("Dictation")),
        "a disabled dictation contributes no heading at all; got {rendered:#?}",
    );
}

/// A resumed transfer says where its bar started. Opening at 38% with
/// nothing said about it reads as a bug rather than as a resume.
#[test]
fn a_resumed_transfer_says_what_it_found() {
    let flat = flatten(&model_rows(
        &App::test_default(),
        &model(
            DictateRole::Transcribing,
            "asr.gguf",
            DictateModelState::Downloading {
                downloaded: 592_000_000,
                total: 1_558_162_944,
                resumed_from: Some(592_000_000),
            },
        ),
        &DictateSnapshot::default(),
        PICKER_WIDTH,
    ));

    assert!(
        flat[0].contains("resuming"),
        "a transfer that picked up from a .part reads as resuming, not downloading; got {:?}",
        flat[0],
    );
    assert!(
        row_containing(&flat, "38%").contains("592 MB / 1.56 GB"),
        "the bar carries real byte counts; got {flat:#?}",
    );
    assert!(
        flat.iter().any(|r| r.contains("resumed from 592 MB found in .part")),
        "the resume line names what was already on disk; got {flat:#?}",
    );
}

/// Cancelling keeps what landed, and the screen says so before forge
/// goes - "cancelled" alone reads as "that 600 MB is gone".
#[test]
fn cancelling_says_what_it_kept_and_where() {
    let text =
        flatten(&dictate_detail(&DictateFailure::Cancelled { kept: 612_000_000 }, PICKER_WIDTH))
            .join(" ");

    assert!(
        text.contains("Nothing was thrown away") && text.contains("612 MB"),
        "the screen says how much survived; got: {text}",
    );
    assert!(
        text.contains(".part") && text.contains("resumes"),
        "and that the next run picks it up; got: {text}",
    );
    assert!(text.contains("forge is quitting"), "and that forge is going; got: {text}");
}

/// A hash mismatch is reported rather than repaired, so the screen owes
/// the reader the command that clears it. Without that this is a screen
/// forge will not leave and will not say how to.
#[test]
fn a_bad_hash_hands_back_the_command_that_clears_it() {
    let text = flatten(&dictate_detail(
        &DictateFailure::HashMismatch {
            path: std::path::PathBuf::from("/models/s1-mini-f16.gguf"),
            expected: "0370da4f1bae19e3150bcafa33c5d396".to_owned(),
            actual: "4f2b9c1a77e0aaaaaaaaaaaaaaaaaaaa".to_owned(),
        },
        PICKER_WIDTH,
    ))
    .join("\n");

    assert!(
        text.contains("s1-mini-f16.gguf hashes to") && text.contains("4f2b9c1a77e0"),
        "the screen states what the file actually hashes to; got:\n{text}",
    );
    assert!(
        text.contains("expected") && text.contains("0370da4f1bae"),
        "and what it should have been; got:\n{text}",
    );
    assert!(
        text.contains("rm /models/s1-mini-f16.gguf"),
        "and the command that clears it, since forge will not delete it itself; got:\n{text}",
    );
    assert!(
        !text.contains("0370da4f1bae19e3150bcafa33c5d396"),
        "64 hex characters do not fit the panel, so both digests are cut; got:\n{text}",
    );
}

/// A row nothing will now start reads `not started`, not `queued`.
/// Queued is a promise a stopped preflight is not keeping.
#[test]
fn a_pending_model_under_a_failure_is_not_queued() {
    let waiting = model(DictateRole::Normalization, "n.gguf", DictateModelState::Pending);
    let running = flatten(&model_rows(
        &App::test_default(),
        &waiting,
        &DictateSnapshot::default(),
        PICKER_WIDTH,
    ));
    assert!(
        running[0].contains("queued"),
        "with work still to come it is queued; got {running:#?}"
    );

    let stopped = flatten(&model_rows(
        &App::test_default(),
        &waiting,
        &DictateSnapshot {
            models: Vec::new(),
            failure: Some(DictateFailure::Cancelled { kept: 0 }),
        },
        PICKER_WIDTH,
    ));
    assert!(
        stopped[0].contains("not started"),
        "once preflight has stopped, nothing is queued; got {stopped:#?}",
    );
}

/// `esc` is only offered while there is a transfer to stop. Offering it
/// once the bytes have landed advertises a key that does nothing.
#[test]
fn the_escape_hint_tracks_whether_there_is_anything_to_cancel() {
    assert_eq!(
        footer_hint(&App::test_default()),
        " ctrl+q  quit",
        "with nothing downloading there is nothing to cancel",
    );
}

/// A command is wrapped rather than elided. Every one of these is on
/// screen because the reader has to run it, and `rm <half a path>` is
/// worse than no command at all - the obvious `truncate_to` is what
/// this exists to keep out.
#[test]
fn a_command_too_long_for_the_panel_wraps_instead_of_truncating() {
    let path = "/Users/somebody/Library/Caches/forge-dictate/s1-mini-f16.gguf";
    let flat = flatten(&command_rows(4, &format!("rm {path}"), PICKER_WIDTH));

    assert!(flat.len() > 1, "this path does not fit one row, so it must use more; got {flat:#?}");
    let rejoined: String =
        flat.iter().map(|r| r.trim()).collect::<Vec<_>>().join("").replace("rm ", "");
    assert_eq!(rejoined, path, "the wrapped rows must rejoin to the exact path; got {flat:#?}");
    assert!(
        !flat.iter().any(|r| r.contains('\u{2026}')),
        "nothing here may be elided; got {flat:#?}",
    );
    assert!(
        flat[1].starts_with("       "),
        "the continuation is indented deeper so it still reads as one command; got {:?}",
        flat[1],
    );
}

/// The three-gigabyte note belongs to a fresh fetch. A resume is
/// finishing a download that already started, and telling that reader
/// they are about to fetch 3.07 GB is wrong about what is happening.
#[test]
fn only_a_fresh_fetch_carries_the_first_run_note() {
    let fresh = model(
        DictateRole::Transcribing,
        "asr.gguf",
        DictateModelState::Downloading { downloaded: 0, total: 100, resumed_from: None },
    );
    let resuming = model(
        DictateRole::Transcribing,
        "asr.gguf",
        DictateModelState::Downloading { downloaded: 40, total: 100, resumed_from: Some(40) },
    );
    assert!(is_first_run_transfer(&fresh), "a transfer from zero is a first run");
    assert!(!is_first_run_transfer(&resuming), "one picking up a .part is not");
    assert!(is_transferring(&resuming), "but it is still a transfer, so esc still cancels it");
}

/// Preflight and the projects view share `PICKER_WIDTH`, which is what
/// makes the handover a content swap rather than a resize. Widening
/// either panel alone puts a visible jump in the middle of boot.
#[test]
fn the_handover_is_a_content_swap_not_a_resize() {
    let flat =
        flatten(&[account_row(&account("Subspace", LoadingState::Ready, "/x"), PICKER_WIDTH)]);
    assert_eq!(
        NAME_WIDTH + 2 + 1 + 1 + STATE_WIDTH + 2,
        usize::from(PICKER_WIDTH),
        "preflight's columns must add up to the picker width it hands over to",
    );
    assert!(
        flat[0].chars().count() < usize::from(PICKER_WIDTH),
        "and no row may exceed it; got {:?}",
        flat[0],
    );
}

/// The bail screen's exits must be ON SCREEN at 34 rows, not merely in
/// the line list. With the wordmark, the five account rows, the two
/// model pairs and the prose, that block is taller than the terminal -
/// so the wordmark is dropped rather than the exits being clipped off
/// the bottom, which is the one outcome this screen cannot have.
#[tokio::test]
async fn a_short_terminal_drops_the_wordmark_rather_than_the_exits() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let config_dir = tempfile::tempdir().expect("tempdir");
    let forge = config_dir.path().join("forge");
    std::fs::create_dir_all(&forge).expect("forge/");
    std::fs::write(
        forge.join("forge.toml"),
        "[[orgs]]\nname = \"Personal\"\n\
         accounts = [\"Subspace\", \"Granite\", \"Granite1\", \"Personal\", \"Codex\"]\n\n\
         [[orgs.projects]]\nname = \"forge\"\npath = \"/tmp\"\n\n\
         [[accounts]]\ndisplay_name = \"Subspace\"\nconfig_dir = \"~/.claude-subspace\"\n\
         [[accounts]]\ndisplay_name = \"Granite\"\nconfig_dir = \"~/.claude-granite\"\n\
         [[accounts]]\ndisplay_name = \"Granite1\"\nconfig_dir = \"~/.claude-granite1\"\n\
         [[accounts]]\ndisplay_name = \"Personal\"\nconfig_dir = \"~/.claude-personal\"\n\
         [[accounts]]\ndisplay_name = \"Codex\"\nconfig_dir = \"~/.claude-codex\"\n",
    )
    .expect("write forge.toml");

    let workspace = forge_workspace::Workspace::new_for_test(config_dir.path().to_owned())
        .await
        .expect("workspace");
    for name in ["Subspace", "Granite", "Personal", "Codex"] {
        workspace.seed_test_account_state(name, LoadingState::Ready);
    }
    workspace.seed_test_account_state("Granite1", LoadingState::Bailed);
    workspace.seed_test_dictate_snapshot(DictateSnapshot {
        models: vec![
            model(
                DictateRole::Transcribing,
                "cohere-transcribe-03-2026-Q4_K_M.gguf",
                DictateModelState::Ready,
            ),
            model(DictateRole::Normalization, "s1-mini-f16.gguf", DictateModelState::Ready),
        ],
        failure: None,
    });
    let mut app = App::test_default();
    app.workspace = Some(std::sync::Arc::new(workspace));

    let mut terminal = Terminal::new(TestBackend::new(100, 34)).expect("terminal");
    terminal.draw(|f| render(f, &mut app)).expect("draw");
    let buf = terminal.backend().buffer();
    let painted: String = (0..34)
        .map(|y| {
            (0..100)
                .map(|x| buf.cell((x, y)).map_or(' ', |c| c.symbol().chars().next().unwrap_or(' ')))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        painted.contains("Fix the auth"),
        "the first exit has to be painted, not merely built:\n{painted}",
    );
    assert!(
        painted.contains("Or drop the account") && painted.contains("[[accounts]]"),
        "and so does the second, which is the one that gets clipped:\n{painted}",
    );
    // Asserted against a row the wordmark genuinely contains - the
    // obvious hand-typed box-drawing needle matches nothing and the
    // assertion silently passes whatever the layout does.
    let wordmark_row =
        "\u{255a}\u{2550}\u{255d}      \u{255a}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{255d}";
    assert!(
        crate::ui::launchpad::wordmark_contains(wordmark_row),
        "this needle must be part of the wordmark, or the assertion below proves nothing",
    );
    assert!(
        !painted.contains(wordmark_row),
        "the wordmark is what gives way, since it is decoration:\n{painted}",
    );
}
