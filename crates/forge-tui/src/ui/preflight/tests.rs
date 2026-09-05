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

/// Draw preflight into a fixed backend and flatten it to text.
fn paint(app: &mut App, width: u16, height: u16) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal.draw(|f| crate::ui::render(f, app)).expect("draw");
    flatten_buffer(terminal.backend().buffer(), width, height)
}

fn flatten_buffer(buf: &ratatui::buffer::Buffer, width: u16, height: u16) -> String {
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buf.cell((x, y)).map_or(' ', |c| c.symbol().chars().next().unwrap_or(' ')))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn model(role: DictateRole, file: &str, state: DictateModelState) -> DictateModel {
    DictateModel { role, file: file.to_owned(), state }
}

/// An `App` whose workspace carries `snapshot` as its dictate state,
/// for assertions that read through the workspace rather than a bare
/// snapshot.
fn app_with_dictate(snapshot: DictateSnapshot) -> App {
    let config_dir = tempfile::tempdir().expect("tempdir");
    let forge = config_dir.path().join("forge");
    std::fs::create_dir_all(&forge).expect("forge/");
    std::fs::write(
        forge.join("forge.toml"),
        "[[orgs]]\nname = \"Personal\"\naccounts = [\"Subspace\"]\n\n\
         [[orgs.projects]]\nname = \"forge\"\npath = \"/tmp\"\n\n\
         [[accounts]]\ndisplay_name = \"Subspace\"\nconfig_dir = \"~/.claude-subspace\"\nprovider = \"anthropic\"\n",
    )
    .expect("write forge.toml");
    let workspace =
        forge_workspace::Workspace::new_for_test(config_dir.path().to_owned()).expect("workspace");
    workspace.seed_test_dictate_snapshot(snapshot);
    let mut app = App::test_default();
    app.workspace = Some(std::sync::Arc::new(workspace));
    app
}

fn account(name: &str, state: LoadingState, dir: &str) -> AccountLoadingRow {
    account_with(name, state, dir, forge_workspace::AccountAuth::Keychain)
}

fn account_with(
    name: &str,
    state: LoadingState,
    dir: &str,
    auth: forge_workspace::AccountAuth,
) -> AccountLoadingRow {
    AccountLoadingRow {
        display_name: name.to_owned(),
        state,
        last_error: None,
        config_dir: std::path::PathBuf::from(dir),
        auth,
    }
}

fn bailed_with_error(
    name: &str,
    dir: &str,
    auth: forge_workspace::AccountAuth,
    last_error: forge_workspace::UsageFetchStatus,
) -> AccountLoadingRow {
    AccountLoadingRow {
        last_error: Some(last_error),
        ..account_with(name, LoadingState::Bailed, dir, auth)
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

/// A bailed account stops forge starting, so the screen has to name
/// BOTH exits: either one alone strands a reader who cannot take that
/// route. They are not equivalent, and the screen says which is which -
/// repairing the account is picked up in place, dropping it is not.
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
    // The two exits are not equivalent and the screen has to say so:
    // this one lands without a restart, the other does not.
    assert!(
        text.contains("forge retries on its own - no restart needed"),
        "the /login exit says the retry is automatic - a reader who fixes their auth otherwise \
         cannot tell whether to restart; got:\n{text}",
    );
    // And it must not name an interval. A keychain account recovers on
    // the 30 s recovery poll, a base-url account on the 60 s usage poll
    // which that poll skips - so any single number is false for one
    // class, and this row cannot tell which it is rendering.
    //
    // Asserted as "this line carries no digit", because enumerating
    // spellings is always one variant short. Scoping to the line is what
    // makes so blunt a property safe: account names and paths carry
    // digits, the retry line has no business carrying one.
    let retry_line = text
        .lines()
        .find(|line| line.contains("forge retries on its own"))
        .expect("the retry line is asserted present above");
    assert!(
        !retry_line.chars().any(|c| c.is_ascii_digit()),
        "no interval belongs on the retry line: the two account classes do not share one; \
         got {retry_line:?}",
    );
}

/// The repair instruction AND the retry line differ by account class.
/// `claude /login` is actively wrong for a base-url account: it has no
/// keychain entry to write, its credential being the token in its own
/// `[accounts.env]`. And the no-restart promise is only true for the
/// keychain arm - an env edit is boot-frozen, so the base-url arm owes
/// the reader the restart instead.
///
/// **Asserted as DIFFERENCES, not as independent contents.** Two
/// `contains` checks would both keep passing if the branches were
/// collapsed and one arm's copy shown to everyone.
#[test]
fn the_repair_and_retry_lines_differ_by_account_class() {
    let render = |auth| {
        flatten(&bail_detail(
            &App::test_default(),
            &account_with("Granite1", LoadingState::Bailed, "/home/x/.claude-granite1", auth),
            PICKER_WIDTH,
        ))
        .join("\n")
    };
    let keychain = render(forge_workspace::AccountAuth::Keychain);
    let base_url = render(forge_workspace::AccountAuth::BaseUrl);

    assert_ne!(
        keychain, base_url,
        "collapsing the classes shows one arm's repair instruction to both; got:\n{keychain}",
    );
    assert!(
        keychain.contains("/login") && !keychain.contains("ANTHROPIC_AUTH_TOKEN"),
        "a keychain account is repaired with /login; got:\n{keychain}",
    );
    assert!(
        keychain.contains("forge retries on its own - no restart needed")
            && !keychain.contains("needs a restart"),
        "a keychain repair is picked up in place; got:\n{keychain}",
    );
    assert!(
        base_url.contains("ANTHROPIC_AUTH_TOKEN in [accounts.env]") && !base_url.contains("/login"),
        "a base-url account has no keychain entry for /login to write; got:\n{base_url}",
    );
    assert!(
        base_url.contains("editing [accounts.env] needs a restart")
            && !base_url.contains("no restart needed"),
        "an env edit is boot-frozen and must not promise an in-place retry; got:\n{base_url}",
    );
    assert!(
        keychain.contains("Or drop the account") && base_url.contains("Or drop the account"),
        "the second exit is class-agnostic; got:\n{keychain}",
    );
}

/// An account whose endpoint is down settles `Bailed` on its own - the
/// loader retries, hits its cap, and stops. Holding preflight after
/// that buys nothing: the launchpad's gate already counts `Bailed` as
/// terminal, the plan excludes the account, and the pollers keep
/// re-probing, so degraded rides along instead of holding boot.
#[tokio::test]
async fn preflight_hands_over_when_an_account_settles_bailed() {
    let config_dir = tempfile::tempdir().expect("tempdir");
    let forge = config_dir.path().join("forge");
    std::fs::create_dir_all(&forge).expect("forge/");
    std::fs::write(
        forge.join("forge.toml"),
        "[[orgs]]\nname = \"Personal\"\naccounts = [\"Subspace\"]\n\n\
         [[orgs.projects]]\nname = \"forge\"\npath = \"/tmp\"\n\n\
         [[accounts]]\ndisplay_name = \"Subspace\"\nconfig_dir = \"~/.claude-subspace\"\nprovider = \"anthropic\"\n",
    )
    .expect("write forge.toml");
    let workspace =
        forge_workspace::Workspace::new_for_test(config_dir.path().to_owned()).expect("workspace");
    let mut app = App::test_default();
    app.workspace = Some(std::sync::Arc::new(workspace));
    app.active_view = crate::app::ActiveView::Launchpad;
    app.startup_project = Some("forge".to_owned());

    crate::app::preflight::tick(&mut app);
    assert!(!app.preflight_done, "a still-loading account holds preflight");

    app.workspace
        .as_ref()
        .expect("workspace")
        .seed_test_account_state("Subspace", LoadingState::Bailed);
    crate::app::preflight::tick(&mut app);
    assert!(app.preflight_done, "a settled account must not hold preflight forever");
    assert_eq!(
        app.active_view,
        crate::app::ActiveView::Chat,
        "the handover goes where the invocation was headed, degraded or not",
    );
}

/// The state column is the typed failure, one label per class. The auth
/// classes keep `auth failed`; a probe that never got through reads
/// `unreachable`; a classed-but-unrecognised failure (a 5xx proxy, a
/// body that will not decode) reads `fetch error`; a 429 streak reads
/// `rate limited`. A red `auth failed` over a healthy token sends the
/// reader to fix the wrong thing.
#[test]
fn the_state_column_names_the_failure_class() {
    let row_text = |last_error: Option<forge_workspace::UsageFetchStatus>| {
        account_row(
            &AccountLoadingRow { last_error, ..account("Subspace", LoadingState::Bailed, "/x") },
            PICKER_WIDTH,
        )
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect::<String>()
    };

    for (status, label) in [
        (Some(forge_workspace::UsageFetchStatus::NetworkFailed), "unreachable"),
        (Some(forge_workspace::UsageFetchStatus::Other), "fetch error"),
        (Some(forge_workspace::UsageFetchStatus::RateLimited), "rate limited"),
        (Some(forge_workspace::UsageFetchStatus::Unauthorized), "auth failed"),
        (None, "auth failed"),
    ] {
        let row = row_text(status);
        assert!(row.trim_end().ends_with(label), "{status:?} must read as {label:?}; got {row:?}");
    }
}

/// The typed label's producer leg, driven through the same
/// `set_last_error` call the loader's retry arm makes: the recorded
/// failure reaches the snapshot, and the snapshot reaches the row.
/// Deleting the snapshot's `last_error` line fails the first assert.
#[tokio::test]
async fn the_recorded_failure_rides_the_snapshot_to_the_row() {
    let config_dir = tempfile::tempdir().expect("tempdir");
    let forge = config_dir.path().join("forge");
    std::fs::create_dir_all(&forge).expect("forge/");
    std::fs::write(
        forge.join("forge.toml"),
        "[[orgs]]\nname = \"Personal\"\naccounts = [\"Subspace\"]\n\n\
         [[orgs.projects]]\nname = \"forge\"\npath = \"/tmp\"\n\n\
         [[accounts]]\ndisplay_name = \"Subspace\"\nconfig_dir = \"~/.claude-subspace\"\nprovider = \"anthropic\"\n",
    )
    .expect("write forge.toml");
    let workspace =
        forge_workspace::Workspace::new_for_test(config_dir.path().to_owned()).expect("workspace");
    workspace.seed_test_account_state("Subspace", LoadingState::Bailed);
    workspace
        .seed_test_account_failure("Subspace", forge_workspace::UsageFetchStatus::NetworkFailed);

    let rows = workspace.account_loading_snapshot();
    assert_eq!(
        rows[0].last_error,
        Some(forge_workspace::UsageFetchStatus::NetworkFailed),
        "the recorded failure reaches the snapshot; got {rows:?}",
    );

    let mut app = App::test_default();
    app.workspace = Some(std::sync::Arc::new(workspace));
    app.active_view = crate::app::ActiveView::Launchpad;
    let painted = paint(&mut app, 100, 34);
    assert!(painted.contains("unreachable"), "and the row renders it as the label:\n{painted}");
}

/// The unreachable screen repairs the endpoint, not the token: the
/// credential is fine, and `Fix the auth` would send a reader with a
/// down proxy off to re-enter a working key.
#[test]
fn an_unreachable_bail_names_the_endpoint_not_the_auth() {
    let text = flatten(&bail_detail(
        &App::test_default(),
        &bailed_with_error(
            "Subspace",
            "/home/x/.claude-subspace",
            forge_workspace::AccountAuth::BaseUrl,
            forge_workspace::UsageFetchStatus::NetworkFailed,
        ),
        PICKER_WIDTH,
    ))
    .join("\n");

    assert!(
        text.contains("Subspace cannot be reached"),
        "the screen says the endpoint is down; got:\n{text}",
    );
    assert!(
        text.contains("forge starts without it"),
        "and that forge no longer holds boot for it; got:\n{text}",
    );
    assert!(
        text.contains("ANTHROPIC_BASE_URL") && !text.contains("Fix the auth"),
        "the repair is the endpoint, never the auth; got:\n{text}",
    );
    assert!(
        text.contains("needs a restart"),
        "the env is read once at boot, so an edited base url does nothing until restart - \
         the screen has to say so; got:\n{text}",
    );
    assert!(
        text.contains("Or drop the account") && text.contains("[[accounts]]"),
        "dropping the account stays as the second way out; got:\n{text}",
    );
}

/// The auth-failure screen for a base-url account promises a restart:
/// its credential is the env token, which the pollers cannot re-read
/// until forge restarts - "recovers in place" would be false.
#[test]
fn a_bailed_base_url_account_promises_a_restart_not_in_place_recovery() {
    let text = flatten(&bail_detail(
        &App::test_default(),
        &bailed_with_error(
            "Subspace",
            "/home/x/.claude-subspace",
            forge_workspace::AccountAuth::BaseUrl,
            forge_workspace::UsageFetchStatus::Unauthorized,
        ),
        PICKER_WIDTH,
    ))
    .join("\n");

    assert!(
        text.contains("Fix the auth"),
        "a 401 on the env token is an auth failure; got:\n{text}",
    );
    assert!(
        text.contains("restart forge to pick"),
        "the env is boot-frozen, so the head line cannot promise in-place recovery; got:\n{text}",
    );
    assert!(
        !text.contains("recovers in place"),
        "the repaired token lives in [accounts.env], read once at boot; got:\n{text}",
    );
}

/// An endpoint that answers badly is not an auth failure either - a
/// proxy with a dead upstream 502s rather than refusing, which is the
/// common real shape of "endpoint down" - and the copy must say the
/// endpoint was reached, because it was.
#[test]
fn an_erroring_endpoint_is_not_an_auth_failure_either() {
    let text = flatten(&bail_detail(
        &App::test_default(),
        &bailed_with_error(
            "Subspace",
            "/home/x/.claude-subspace",
            forge_workspace::AccountAuth::BaseUrl,
            forge_workspace::UsageFetchStatus::Other,
        ),
        PICKER_WIDTH,
    ))
    .join("\n");

    assert!(
        text.contains("Subspace keeps failing its probe"),
        "the head claims only that the probe failed - the class covers endpoints that \
         answered badly and probes that could not run; got:\n{text}",
    );
    assert!(
        text.contains("Check the endpoint")
            && !text.contains("Fix the auth")
            && !text.contains("ANTHROPIC_AUTH_TOKEN"),
        "the repair is the endpoint, never the token; got:\n{text}",
    );
}

/// A bailed token account's credential is the setup token in its
/// `[accounts.env]`, not a keychain entry: `/login` would authenticate
/// whichever account owns the shared config dir, not this one. The
/// repair is a re-mint, and it is an env edit, so it needs a restart.
#[test]
fn a_bailed_token_account_names_the_re_mint_not_login() {
    let text = flatten(&bail_detail(
        &App::test_default(),
        &bailed_with_error(
            "TokenAcct",
            "/home/x/.claude",
            forge_workspace::AccountAuth::Token,
            forge_workspace::UsageFetchStatus::Unauthorized,
        ),
        PICKER_WIDTH,
    ))
    .join("\n");

    assert!(
        text.contains("Fix the auth"),
        "a 401 on a setup token is an auth failure; got:\n{text}",
    );
    assert!(
        text.contains("restart forge to pick"),
        "a token repair is an env edit, so the head line cannot promise in-place recovery; got:\n{text}",
    );
    assert!(
        !text.contains("/login"),
        "`/login` repairs the shared dir's keychain account, never this one; got:\n{text}",
    );
    assert!(
        text.contains("CLAUDE_CODE_OAUTH_TOKEN in [accounts.env]"),
        "the credential's home is named; got:\n{text}",
    );
    assert!(text.contains("claude setup-token"), "the re-mint command is the repair; got:\n{text}");
    assert!(
        text.contains("editing [accounts.env] needs a restart"),
        "an env repair is boot-frozen until restart - the screen has to say so; got:\n{text}",
    );
}

/// A 429 streak is nobody's repair job: the token is fine and the
/// endpoint is fine, so the only instruction is to wait.
#[test]
fn a_rate_limited_bail_tells_the_reader_to_wait() {
    let text = flatten(&bail_detail(
        &App::test_default(),
        &bailed_with_error(
            "Subspace",
            "/home/x/.claude-subspace",
            forge_workspace::AccountAuth::BaseUrl,
            forge_workspace::UsageFetchStatus::RateLimited,
        ),
        PICKER_WIDTH,
    ))
    .join("\n");

    assert!(text.contains("Subspace is rate limited"), "the head names the limit; got:\n{text}");
    assert!(text.contains("Waiting clears it"), "the repair is time, not an edit; got:\n{text}");
    assert!(
        !text.contains("Fix the auth")
            && !text.contains("ANTHROPIC_AUTH_TOKEN")
            && !text.contains("Check the endpoint"),
        "neither the token nor the endpoint is the problem; got:\n{text}",
    );
}

/// The unreachable repair line is class-shaped like the auth one: a
/// keychain account has no base url to check, so naming
/// `ANTHROPIC_BASE_URL` at it would send a reader hunting for a key
/// their forge.toml does not have.
///
/// **Asserted as a DIFFERENCE, not as two independent contents**, for
/// the same reason the auth repair is: two `contains` checks would both
/// keep passing if the branch were collapsed and one arm's line shown
/// to everyone.
#[test]
fn the_unreachable_repair_differs_by_account_class() {
    let render = |auth| {
        flatten(&bail_detail(
            &App::test_default(),
            &bailed_with_error(
                "Subspace",
                "/home/x/.claude-subspace",
                auth,
                forge_workspace::UsageFetchStatus::NetworkFailed,
            ),
            PICKER_WIDTH,
        ))
        .join("\n")
    };
    let keychain = render(forge_workspace::AccountAuth::Keychain);
    let base_url = render(forge_workspace::AccountAuth::BaseUrl);

    assert_ne!(
        keychain, base_url,
        "collapsing the classes shows one arm's repair line to both; got:\n{keychain}",
    );
    assert!(
        keychain.contains("Anthropic API") && !keychain.contains("ANTHROPIC_BASE_URL"),
        "a keychain account has no base url to check; got:\n{keychain}",
    );
    assert!(
        base_url.contains("ANTHROPIC_BASE_URL") && !base_url.contains("Anthropic API"),
        "a base-url account's endpoint is the thing to check; got:\n{base_url}",
    );
}

/// `Bailed` is red rather than the shipped warning yellow. On the one
/// screen that gates forge starting, mid-flight and failed must not
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
    let text = flatten(&dictate_detail(
        &DictateFailure::Cancelled { kept: 612_000_000, total: 1_558_162_944 },
        PICKER_WIDTH,
    ))
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

/// Preflight runs on every route and hands over to wherever the user
/// was headed: the project picker for `forge`, chat for
/// `forge <project>`.
///
/// Driven through `crate::ui::render` rather than through
/// `preflight::render` directly, because the whole family of defects
/// this closes lived in the branch that chooses between the two views -
/// a screen frozen with a dead spinner, a boot screen reachable
/// mid-session with no way out, and exits clipped off the bottom all hid
/// in the one gap where nothing exercised it.
#[tokio::test]
async fn preflight_renders_on_both_routes_and_hands_over_to_each() {
    for startup_project in [None, Some("forge".to_owned())] {
        let config_dir = tempfile::tempdir().expect("tempdir");
        let forge = config_dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge/");
        std::fs::write(
            forge.join("forge.toml"),
            "[[orgs]]\nname = \"Personal\"\naccounts = [\"Subspace\"]\n\n\
             [[orgs.projects]]\nname = \"forge\"\npath = \"/tmp\"\n\n\
             [[accounts]]\ndisplay_name = \"Subspace\"\nconfig_dir = \"~/.claude-subspace\"\nprovider = \"anthropic\"\n",
        )
        .expect("write forge.toml");
        let workspace = forge_workspace::Workspace::new_for_test(config_dir.path().to_owned())
            .expect("workspace");

        let mut app = App::test_default();
        app.workspace = Some(std::sync::Arc::new(workspace));
        app.active_view = crate::app::ActiveView::Launchpad;
        app.startup_project = startup_project.clone();

        // A fresh account map starts every account Loading, so preflight
        // has something to wait on.
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 34)).expect("terminal");
        terminal.draw(|f| crate::ui::render(f, &mut app)).expect("draw");
        let painted = flatten_buffer(terminal.backend().buffer(), 100, 34);
        assert!(
            painted.contains("Accounts") && painted.contains("resolving"),
            "preflight is what renders while accounts resolve, on every route:\n{painted}",
        );
        crate::app::preflight::tick(&mut app);
        assert!(!app.preflight_done, "and it does not hand over while one is still resolving");

        app.workspace
            .as_ref()
            .expect("workspace")
            .seed_test_account_state("Subspace", LoadingState::Ready);
        crate::app::preflight::tick(&mut app);
        assert!(app.preflight_done, "once every account is ready it hands over");

        let expected = if startup_project.is_some() {
            crate::app::ActiveView::Chat
        } else {
            crate::app::ActiveView::Launchpad
        };
        assert_eq!(
            app.active_view, expected,
            "a named project lands in chat and no project lands on the picker; \
             startup_project was {startup_project:?}",
        );

        // Keyed on preflight's own heading, which the picker never
        // draws. The obvious needle is "resolving", and it discriminates
        // the ACCOUNT STATE rather than the view: by this point the
        // account is Ready, so preflight would print "ready" and the
        // assertion would pass whichever view painted.
        let painted = paint(&mut app, 100, 34);
        assert!(
            !painted.contains("Accounts"),
            "the handover has to change what renders, not just what it says; \
             startup_project was {startup_project:?}:\n{painted}",
        );
    }
}

/// Cancelling quits forge, so the frame that says what was kept has to
/// have reached the buffer first. Setting the flag from having RUN
/// rather than from having PAINTED lets a panel too small to draw its
/// body quit having said nothing - which is the whole of what the user
/// pressed escape to be told.
#[tokio::test]
async fn forge_does_not_quit_on_cancel_until_the_copy_is_on_screen() {
    let config_dir = tempfile::tempdir().expect("tempdir");
    let forge = config_dir.path().join("forge");
    std::fs::create_dir_all(&forge).expect("forge/");
    std::fs::write(
        forge.join("forge.toml"),
        "[[orgs]]\nname = \"Personal\"\naccounts = [\"Subspace\"]\n\n\
         [[orgs.projects]]\nname = \"forge\"\npath = \"/tmp\"\n\n\
         [[accounts]]\ndisplay_name = \"Subspace\"\nconfig_dir = \"~/.claude-subspace\"\nprovider = \"anthropic\"\n",
    )
    .expect("write forge.toml");
    let workspace =
        forge_workspace::Workspace::new_for_test(config_dir.path().to_owned()).expect("workspace");
    workspace.seed_test_account_state("Subspace", LoadingState::Ready);
    workspace.seed_test_dictate_snapshot(DictateSnapshot {
        models: vec![model(
            DictateRole::Transcribing,
            "asr.gguf",
            DictateModelState::Failed(DictateFailure::Cancelled {
                kept: 612_000_000,
                total: 1_558_162_944,
            }),
        )],
        failure: Some(DictateFailure::Cancelled { kept: 612_000_000, total: 1_558_162_944 }),
    });
    let mut app = App::test_default();
    app.workspace = Some(std::sync::Arc::new(workspace));
    app.active_view = crate::app::ActiveView::Launchpad;

    // Two rows of panel is the framing rules and nothing between them.
    let painted = paint(&mut app, 100, 4);
    assert!(
        !painted.contains("Nothing was thrown away"),
        "this size must genuinely fail to show the copy, or the assertion below is vacuous:\n\
         {painted}",
    );
    assert!(
        !app.preflight_cancel_drawn,
        "a frame that could not paint the body has not said anything to quit on",
    );
    crate::app::preflight::tick(&mut app);
    assert!(!app.should_quit, "so forge waits rather than vanishing with the message unshown");

    let painted = paint(&mut app, 100, 34);
    assert!(
        painted.contains("Nothing was thrown away") && painted.contains("612 MB"),
        "at a normal size the copy is on screen:\n{painted}",
    );
    assert!(app.preflight_cancel_drawn, "and the flag follows what was painted");
    crate::app::preflight::tick(&mut app);
    assert!(app.should_quit, "having said it, forge goes");
}

/// A cancelled transfer must not read as a finished one. The bar keeps
/// the fraction it reached and the footer stops offering keys - a full
/// orange bar beside `cancelled`, or an `esc  cancel` under it, both say
/// the opposite of what happened.
#[test]
fn a_cancelled_transfer_does_not_read_as_a_finished_one() {
    let snapshot = DictateSnapshot {
        models: vec![model(
            DictateRole::Transcribing,
            "asr.gguf",
            DictateModelState::Failed(DictateFailure::Cancelled {
                kept: 592_000_000,
                total: 1_558_162_944,
            }),
        )],
        failure: Some(DictateFailure::Cancelled { kept: 592_000_000, total: 1_558_162_944 }),
    };
    let flat =
        flatten(&model_rows(&App::test_default(), &snapshot.models[0], &snapshot, PICKER_WIDTH));

    let bar = row_containing(&flat, "\u{2588}");
    assert!(
        bar.contains("38%") && bar.contains("592 MB / 1.56 GB"),
        "the bar keeps the fraction it reached, so the screen agrees with the prose; got {bar:?}",
    );
    assert!(
        bar.contains('\u{2591}'),
        "a bar with no empty cells left reads as a completed download; got {bar:?}",
    );
}

/// A failed row wears its own cause, never the snapshot's. Each row
/// here is rendered against a snapshot naming the OTHER model's cause:
/// nothing the row renders may come from it.
#[test]
fn a_failed_row_wears_its_own_cause_not_the_snapshots() {
    let cancelled = DictateFailure::Cancelled { kept: 592_000_000, total: 1_558_162_944 };
    let hash = DictateFailure::HashMismatch {
        path: std::path::PathBuf::from("/models/s1-mini-f16.gguf"),
        expected: "0370da4f1bae".to_owned(),
        actual: "4f2b9c1a77e0".to_owned(),
        size: 1_509_347_232,
    };
    let app = App::test_default();

    let flat = flatten(&model_rows(
        &app,
        &model(DictateRole::Transcribing, "asr.gguf", DictateModelState::Failed(cancelled.clone())),
        &DictateSnapshot { models: Vec::new(), failure: Some(hash.clone()) },
        PICKER_WIDTH,
    ));
    let row = row_containing(&flat, "transcribing model");
    assert!(
        row.contains("cancelled"),
        "the row names its own cause, not the snapshot's; got {flat:#?}",
    );
    let bar = row_containing(&flat, "\u{2588}");
    assert!(
        bar.contains("38%") && bar.contains("592 MB / 1.56 GB"),
        "the bar carries this transfer's bytes, not the snapshot's; got {bar:?}",
    );

    let flat = flatten(&model_rows(
        &app,
        &model(DictateRole::Normalization, "s1-mini-f16.gguf", DictateModelState::Failed(hash)),
        &DictateSnapshot { models: Vec::new(), failure: Some(cancelled) },
        PICKER_WIDTH,
    ));
    let row = row_containing(&flat, "normalization model");
    assert!(
        row.contains("bad hash"),
        "and the mirror: this row names its own cause too; got {flat:#?}",
    );
    assert!(
        !flat.iter().any(|r| r.contains("1.56 GB")),
        "another model's byte counts must not reach this row; got {flat:#?}",
    );
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
            size: 1_509_347_232,
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
    // Whitespace-normalised: the prose wraps to the panel, so a phrase
    // that straddles a line break is still present.
    let flowed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flowed.contains("throwing away a 1.51 GB file"),
        "the size is what makes 'not forge's call' a reason rather than an assertion; got:\n{text}",
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
            failure: Some(DictateFailure::Cancelled { kept: 0, total: 0 }),
        },
        PICKER_WIDTH,
    ));
    assert!(
        stopped[0].contains("not started"),
        "once preflight has stopped, nothing is queued; got {stopped:#?}",
    );
}

/// `esc` is only offered while there is a transfer to stop. Offering it
/// once the bytes have landed advertises a key that does nothing, and
/// once a cancellation has fired forge is on its way out - the footer
/// offers no keys at all.
#[tokio::test]
async fn the_escape_hint_tracks_whether_there_is_anything_to_cancel() {
    assert_eq!(
        footer_hint(&App::test_default()),
        " ctrl+q  quit",
        "with nothing downloading there is nothing to cancel",
    );

    let transferring = app_with_dictate(DictateSnapshot {
        models: vec![model(
            DictateRole::Transcribing,
            "asr.gguf",
            DictateModelState::Downloading { downloaded: 0, total: 1, resumed_from: Some(0) },
        )],
        failure: None,
    });
    assert_eq!(
        footer_hint(&transferring),
        " esc  cancel     ctrl+q  quit",
        "bytes moving is what makes esc mean something",
    );

    let cancelled = app_with_dictate(DictateSnapshot {
        models: Vec::new(),
        failure: Some(DictateFailure::Cancelled { kept: 0, total: 0 }),
    });
    assert_eq!(
        footer_hint(&cancelled),
        " quitting\u{2026}",
        "a cancelled preflight is leaving; no keys are offered",
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

/// Wrapping must terminate on non-ASCII, at every width the panel can
/// reach. Measuring the row budget in characters and cutting with a byte
/// index agrees on ASCII and diverges the moment a path is not: the cut
/// lands at zero, the remainder never shrinks, and the render thread
/// spins pushing empty rows with the terminal in raw mode.
///
/// A config dir, a models dir or a home directory with one accented or
/// CJK character is all it takes, and `panel_width` follows the terminal
/// down to nothing.
#[test]
fn wrapping_terminates_and_rejoins_on_non_ascii_paths() {
    for text in [
        "rm ~/\u{c9}t\u{e9}/mod\u{e8}les/forge-dictate/s1-mini-f16.gguf",
        "rm ~/\u{30e2}\u{30c7}\u{30eb}/forge-dictate/s1-mini-f16.gguf",
        "rm ~/Models/forge-dictate/s1-mini-f16.gguf",
    ] {
        for width in [1u16, 2, 4, 6, 8, 10, 12, 16, 20, 32, PICKER_WIDTH] {
            let rows = command_rows(4, text, width);
            // The payload span, not the whole row: at a width of one the
            // command's own space character IS a row, and trimming rows
            // would drop it and read as loss.
            let payloads: Vec<&str> =
                rows.iter().map(|row| row.spans[1].content.as_ref()).collect();
            assert_eq!(
                payloads.concat(),
                text,
                "every character must survive the wrap at width {width}; got {payloads:#?}",
            );
            assert!(
                !payloads.iter().any(|p| p.is_empty()),
                "an empty row means the cut did not advance at width {width}; got {payloads:#?}",
            );
        }
    }
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
    let config_dir = tempfile::tempdir().expect("tempdir");
    let forge = config_dir.path().join("forge");
    std::fs::create_dir_all(&forge).expect("forge/");
    std::fs::write(
        forge.join("forge.toml"),
        "[[orgs]]\nname = \"Personal\"\n\
         accounts = [\"Subspace\", \"Granite\", \"Granite1\", \"Personal\", \"Codex\"]\n\n\
         [[orgs.projects]]\nname = \"forge\"\npath = \"/tmp\"\n\n\
         [[accounts]]\ndisplay_name = \"Subspace\"\nconfig_dir = \"~/.claude-subspace\"\nprovider = \"anthropic\"\n\
         [[accounts]]\ndisplay_name = \"Granite\"\nconfig_dir = \"~/.claude-granite\"\nprovider = \"anthropic\"\n\
         [[accounts]]\ndisplay_name = \"Granite1\"\nconfig_dir = \"~/.claude-granite1\"\nprovider = \"anthropic\"\n\
         [[accounts]]\ndisplay_name = \"Personal\"\nconfig_dir = \"~/.claude-personal\"\nprovider = \"anthropic\"\n\
         [[accounts]]\ndisplay_name = \"Codex\"\nconfig_dir = \"~/.claude-codex\"\nprovider = \"anthropic\"\n",
    )
    .expect("write forge.toml");

    let workspace =
        forge_workspace::Workspace::new_for_test(config_dir.path().to_owned()).expect("workspace");
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
    app.active_view = crate::app::ActiveView::Launchpad;

    // Swept rather than checked at one size: the bailed screen is taller
    // than the terminal well before 34 rows, and clipping took the END
    // of the body - which is where the exits are. Measured before the
    // fix: 100x24 lost the second exit, 100x20 lost both commands.
    for height in [20u16, 24, 28, 34, 50] {
        let painted = paint(&mut app, 100, height);
        assert!(
            painted.contains("Fix the auth"),
            "the first exit has to be painted, not merely built, at {height} rows:\n{painted}",
        );
        assert!(
            painted.contains("Or drop the account") && painted.contains("[[accounts]]"),
            "and so does the second, which is the one that got clipped, at {height} rows:\n\
             {painted}",
        );
    }
    let painted = paint(&mut app, 100, 34);
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
