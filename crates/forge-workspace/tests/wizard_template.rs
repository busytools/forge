//! The wizard template is executable code that ships to every install.
//!
//! These tests slice its library at the authoring marker and drive bash
//! against a scratch directory, so the helpers a wizard author calls are
//! exercised rather than only read.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The asset, read from the crate that ships it.
const TEMPLATE: &str = include_str!("../src/wizard/template.sh");

/// The helpers below run outside a `#[test]` body, where clippy's in-tests
/// exemption does not reach, so they return their failures instead of
/// carrying a blanket allow for this file.
type TestResult<T> = Result<T, Box<dyn std::error::Error>>;

/// The library above the authoring marker, written into `dir`, so the example
/// stage never runs and a child with a restricted PATH can still source it.
fn library(dir: &Path) -> TestResult<PathBuf> {
    let Some((library, _)) = TEMPLATE.split_once("# STAGES: author") else {
        return Err("the template keeps its authoring marker".into());
    };
    let path = dir.join("wizard-library.sh");
    std::fs::write(&path, library)?;
    Ok(path)
}

/// A bash command with the library sourced, in `dir`, stdin at EOF.
fn command(dir: &Path, body: &str) -> TestResult<Command> {
    let path = library(dir)?;
    let mut command = Command::new("bash");
    command
        .arg("-c")
        .arg(format!("source \"$WIZARD_LIBRARY\"\n{body}"))
        .env("WIZARD_LIBRARY", path)
        .current_dir(dir)
        .stdin(Stdio::null());
    Ok(command)
}

fn run(dir: &Path, body: &str, env: &[(&str, &str)]) -> TestResult<Output> {
    let mut command = command(dir, body)?;
    for (key, value) in env {
        command.env(key, value);
    }
    Ok(command.output()?)
}

/// Run `body` with `input` fed to its stdin.
fn run_with_input(dir: &Path, body: &str, input: &str) -> TestResult<Output> {
    let mut command = command(dir, body)?;
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take().ok_or("stdin is piped")?;
    stdin.write_all(input.as_bytes())?;
    drop(stdin);
    Ok(child.wait_with_output()?)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A stub `gh` that reports signed in and fails one write command. Returns the
/// PATH to run with.
fn stub_gh(dir: &Path, failing: &str) -> TestResult<String> {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin)?;
    let gh = bin.join("gh");
    std::fs::write(
        &gh,
        format!(
            "#!/bin/sh\ncase \"$1 $2\" in\n  \"auth status\") exit 0;;\n  \"{failing}\") exit 1;;\nesac\nexit 0\n"
        ),
    )?;
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755))?;
    let path = std::env::var("PATH")?;
    Ok(format!("{}:{path}", bin.display()))
}

/// **A key is matched exactly and validated first.** Interpolated into a
/// pattern, `A.B` would delete an unrelated line and `A[` would empty the
/// file; neither may reach the file at all.
#[test]
fn refuses_a_bad_key_and_leaves_the_env_file_alone() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let env_file = scratch.path().join(".env");
    std::fs::write(&env_file, "A=1\nA_OLD=keep\nA.B=keep\nB=2\n").expect("write .env");
    let before = std::fs::read_to_string(&env_file).expect("read");

    let bracket = run(scratch.path(), "write_env 'A[' x", &[]).expect("run bash");
    assert!(!bracket.status.success(), "A[ is refused: {}", stdout(&bracket));
    assert_eq!(std::fs::read_to_string(&env_file).expect("read"), before, "A[ leaves .env intact");

    let dotted = run(scratch.path(), "write_env 'A.B' x", &[]).expect("run bash");
    assert!(!dotted.status.success(), "A.B is refused: {}", stdout(&dotted));
    assert_eq!(std::fs::read_to_string(&env_file).expect("read"), before, "A.B leaves .env intact");
}

/// One exact-field upsert: a comment and the keys that merely contain the
/// name survive, and the named key's old line goes.
#[test]
fn upserts_one_key_and_keeps_its_neighbours() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let env_file = scratch.path().join(".env");
    std::fs::write(&env_file, "# a comment\nFOO_BAR=old\nXFOO=keep\nFOO=old\n").expect("write");

    let output = run(scratch.path(), "write_env FOO new", &[]).expect("run bash");
    assert!(output.status.success(), "the write succeeds: {}", stdout(&output));
    assert_eq!(
        std::fs::read_to_string(&env_file).expect("read"),
        "# a comment\nFOO_BAR=old\nXFOO=keep\nFOO=new\n",
    );
}

/// A value that spans lines cannot live in an env file, and the refusal says
/// where such a value belongs.
#[test]
fn refuses_a_multiline_value_and_leaves_the_env_file_alone() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let env_file = scratch.path().join(".env");
    std::fs::write(&env_file, "A=1\n").expect("write");

    let output = run(scratch.path(), r"write_env MULTI $'one\ntwo'", &[]).expect("run bash");
    assert!(!output.status.success(), "a newline value is refused: {}", stdout(&output));
    assert!(
        stdout(&output).contains("multi-line value in the file its program reads"),
        "the refusal names the destination: {}",
        stdout(&output),
    );
    assert_eq!(std::fs::read_to_string(&env_file).expect("read"), "A=1\n");
}

/// The value a prompt offers on a re-run comes back exactly, spaces and all.
#[test]
fn reads_a_value_back_exactly() {
    let scratch = tempfile::tempdir().expect("tempdir");
    std::fs::write(scratch.path().join(".env"), "SPACED=a b  c\n").expect("write");

    let output = run(scratch.path(), "_existing SPACED", &[]).expect("run bash");
    assert!(output.status.success(), "the key is found: {}", stdout(&output));
    assert_eq!(stdout(&output), "a b  c", "the value is unchanged by the read");
}

/// **A miss is a miss, not a read failure.** An absent file and a file
/// without the key both let the prompt through.
#[test]
fn ask_prompts_when_the_key_is_absent() {
    let scratch = tempfile::tempdir().expect("tempdir");

    let absent = run(scratch.path(), "ask FOO \"value:\"", &[]).expect("run bash");
    assert!(absent.status.success(), "no .env is a miss: {}", stdout(&absent));
    assert!(stdout(&absent).contains("value:"), "the prompt is shown: {}", stdout(&absent));
    assert!(!stdout(&absent).contains("could not read"), "{}", stdout(&absent));

    std::fs::write(scratch.path().join(".env"), "OTHER=1\n").expect("write .env");
    let missing = run(scratch.path(), "ask FOO \"value:\"", &[]).expect("run bash");
    assert!(missing.status.success(), "a key not in the file is a miss: {}", stdout(&missing));
    assert!(stdout(&missing).contains("value:"), "the prompt is shown: {}", stdout(&missing));
    assert!(!stdout(&missing).contains("could not read"), "{}", stdout(&missing));
}

/// **The upsert's own read failure stops before anything is written.** The
/// awk status is the only thing between a failed read and an emptied file.
#[test]
fn write_env_stops_when_it_cannot_read_the_env_file() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let env_file = scratch.path().join(".env");
    std::fs::write(&env_file, "A=keep\n").expect("write");
    let before = std::fs::read(&env_file).expect("read");
    std::fs::set_permissions(&env_file, std::fs::Permissions::from_mode(0o000)).expect("mode");

    let output = run(scratch.path(), "write_env FOO new", &[]).expect("run bash");
    std::fs::set_permissions(&env_file, std::fs::Permissions::from_mode(0o644)).expect("mode");

    assert!(!output.status.success(), "the write stops: {}", stdout(&output));
    assert!(
        stdout(&output).contains("could not read"),
        "the read failure is named: {}",
        stdout(&output),
    );
    assert_eq!(std::fs::read(&env_file).expect("read"), before, "the file is byte-identical");
}

/// **A signed-in gh that fails the write stops the wizard**, naming the
/// secret: nothing is recorded, so a re-run asks again.
#[test]
fn a_failing_gh_stops_the_wizard_with_its_message() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let with_stub = stub_gh(scratch.path(), "secret set").expect("stub gh");

    let output =
        run(scratch.path(), "set_secret MY_SECRET hunter2", &[("PATH", with_stub.as_str())])
            .expect("run bash");
    assert!(!output.status.success(), "the failure stops the run: {}", stdout(&output));
    assert!(
        stdout(&output).contains("GitHub secret MY_SECRET was not set"),
        "the secret is named: {}",
        stdout(&output),
    );
}

/// `set_var`'s failure path is its twin's: name the variable and stop.
#[test]
fn a_failing_gh_stops_set_var_with_its_message() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let with_stub = stub_gh(scratch.path(), "variable set").expect("stub gh");

    let output = run(scratch.path(), "set_var MY_VAR hunter2", &[("PATH", with_stub.as_str())])
        .expect("run bash");
    assert!(!output.status.success(), "the failure stops the run: {}", stdout(&output));
    assert!(
        stdout(&output).contains("GitHub variable MY_VAR was not set"),
        "the variable is named: {}",
        stdout(&output),
    );
}

/// `confirm` answers no loudly, and the if/else idiom carries on.
#[test]
fn confirm_states_its_no_and_the_idiom_carries_on() {
    let scratch = tempfile::tempdir().expect("tempdir");

    let bare = run(scratch.path(), "confirm \"delete it?\"", &[]).expect("run bash");
    assert!(!bare.status.success(), "a bare confirm returns non-zero on no");
    assert!(stdout(&bare).contains("answered no: delete it?"), "{}", stdout(&bare));

    let idiom = run(
        scratch.path(),
        "if confirm \"delete it?\"; then echo took; else echo no-branch; fi",
        &[],
    )
    .expect("run bash");
    assert!(idiom.status.success(), "the idiom keeps the run going: {}", stdout(&idiom));
    assert!(stdout(&idiom).contains("no-branch"), "{}", stdout(&idiom));
}

/// `confirm` takes yes on a `y`, and the idiom takes its first arm.
#[test]
fn confirm_accepts_yes() {
    let scratch = tempfile::tempdir().expect("tempdir");

    let bare = run_with_input(scratch.path(), "confirm \"keep it?\"", "y\n").expect("run bash");
    assert!(bare.status.success(), "y is a yes: {}", stdout(&bare));
    assert!(!stdout(&bare).contains("answered no"), "{}", stdout(&bare));

    let idiom = run_with_input(
        scratch.path(),
        "if confirm \"keep it?\"; then echo took; else echo no-branch; fi",
        "y\n",
    )
    .expect("run bash");
    assert!(idiom.status.success(), "the idiom keeps the run going: {}", stdout(&idiom));
    assert!(stdout(&idiom).contains("took"), "the yes arm runs: {}", stdout(&idiom));
    assert!(!stdout(&idiom).contains("no-branch"), "{}", stdout(&idiom));
}

/// **A read failure reaches the person from `ask`.** Inside the command
/// substitution, `_stop` would only end the subshell and its message would be
/// captured as the prompt's current value.
#[test]
fn an_unreadable_env_file_stops_ask_with_its_own_message() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let env_file = scratch.path().join(".env");
    std::fs::write(&env_file, "A=1\n").expect("write");
    std::fs::set_permissions(&env_file, std::fs::Permissions::from_mode(0o000)).expect("mode");

    let output = run(scratch.path(), "ask FOO \"value:\"", &[]).expect("run bash");
    std::fs::set_permissions(&env_file, std::fs::Permissions::from_mode(0o644)).expect("mode");

    assert!(!output.status.success(), "ask stops: {}", stdout(&output));
    assert!(
        stdout(&output).contains("could not read"),
        "the wizard's own message is delivered: {}",
        stdout(&output),
    );
}

/// `ask_secret` guards its read the same way `ask` does.
#[test]
fn an_unreadable_env_file_stops_ask_secret_with_the_same_message() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let env_file = scratch.path().join(".env");
    std::fs::write(&env_file, "A=1\n").expect("write");
    std::fs::set_permissions(&env_file, std::fs::Permissions::from_mode(0o000)).expect("mode");

    let output = run(scratch.path(), "ask_secret FOO \"value:\"", &[]).expect("run bash");
    std::fs::set_permissions(&env_file, std::fs::Permissions::from_mode(0o644)).expect("mode");

    assert!(!output.status.success(), "ask_secret stops: {}", stdout(&output));
    assert!(
        stdout(&output).contains("could not read"),
        "the wizard's own message is delivered: {}",
        stdout(&output),
    );
}
