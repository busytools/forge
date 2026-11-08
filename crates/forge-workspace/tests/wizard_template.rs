//! The wizard template is executable code that ships to every install.
//!
//! These tests slice its library at the authoring marker and drive bash
//! against a scratch directory, so the helpers a wizard author calls are
//! exercised rather than only read.

// An integration test is a crate of its own, so clippy's test exemption does
// not reach it: the denied lints fire on a file that is entirely test code.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// The asset, read from the crate that ships it.
const TEMPLATE: &str = include_str!("../src/wizard/template.sh");

/// Run `body` with the template's library sourced, in `dir`, with stdin at
/// EOF. The library alone is written, so the example stage never runs.
fn run(dir: &Path, body: &str, env: &[(&str, &str)]) -> Output {
    let library = TEMPLATE
        .split_once("# STAGES: author")
        .map(|(library, _)| library)
        .expect("the template keeps its authoring marker");
    let path = dir.join("wizard-library.sh");
    std::fs::write(&path, library).expect("write the library");
    let mut command = Command::new("bash");
    command
        .arg("-c")
        .arg(format!("source \"$WIZARD_LIBRARY\"\n{body}"))
        .env("WIZARD_LIBRARY", &path)
        .current_dir(dir)
        .stdin(Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("run bash")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
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

    let bracket = run(scratch.path(), "write_env 'A[' x", &[]);
    assert!(!bracket.status.success(), "A[ is refused: {}", stdout(&bracket));
    assert_eq!(std::fs::read_to_string(&env_file).expect("read"), before, "A[ leaves .env intact");

    let dotted = run(scratch.path(), "write_env 'A.B' x", &[]);
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

    let output = run(scratch.path(), "write_env FOO new", &[]);
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

    let output = run(scratch.path(), r"write_env MULTI $'one\ntwo'", &[]);
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

    let output = run(scratch.path(), "_existing SPACED", &[]);
    assert!(output.status.success(), "the key is found: {}", stdout(&output));
    assert_eq!(stdout(&output), "a b  c", "the value is unchanged by the read");
}

/// **A signed-in gh that fails the write stops the wizard**, naming the
/// secret: nothing is recorded, so a re-run asks again.
#[test]
fn a_failing_gh_stops_the_wizard_with_its_message() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let bin = scratch.path().join("bin");
    std::fs::create_dir(&bin).expect("bin dir");
    let gh = bin.join("gh");
    std::fs::write(
        &gh,
        "#!/bin/sh\ncase \"$1 $2\" in\n  \"auth status\") exit 0;;\n  \"secret set\") exit 1;;\nesac\nexit 0\n",
    )
    .expect("stub gh");
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).expect("mode");
    let path = std::env::var("PATH").expect("PATH is set");
    let with_stub = format!("{}:{path}", bin.display());

    let output =
        run(scratch.path(), "set_secret MY_SECRET hunter2", &[("PATH", with_stub.as_str())]);
    assert!(!output.status.success(), "the failure stops the run: {}", stdout(&output));
    assert!(
        stdout(&output).contains("GitHub secret MY_SECRET was not set"),
        "the secret is named: {}",
        stdout(&output),
    );
}

/// `confirm` answers no loudly, and the if/else idiom carries on.
#[test]
fn confirm_states_its_no_and_the_idiom_carries_on() {
    let scratch = tempfile::tempdir().expect("tempdir");

    let bare = run(scratch.path(), "confirm \"delete it?\"", &[]);
    assert!(!bare.status.success(), "a bare confirm returns non-zero on no");
    assert!(stdout(&bare).contains("answered no: delete it?"), "{}", stdout(&bare));

    let idiom = run(
        scratch.path(),
        "if confirm \"delete it?\"; then echo took; else echo no-branch; fi",
        &[],
    );
    assert!(idiom.status.success(), "the idiom keeps the run going: {}", stdout(&idiom));
    assert!(stdout(&idiom).contains("no-branch"), "{}", stdout(&idiom));
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

    let output = run(scratch.path(), "ask FOO \"value:\"", &[]);
    std::fs::set_permissions(&env_file, std::fs::Permissions::from_mode(0o644)).expect("mode");

    assert!(!output.status.success(), "ask stops: {}", stdout(&output));
    assert!(
        stdout(&output).contains("could not read"),
        "the wizard's own message is delivered: {}",
        stdout(&output),
    );
}
