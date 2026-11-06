//! The real shells: the OSC 133 marks, end to end through a PTY.
//!
//! A shell forge knows must report a command's start and its exit status, and
//! must not report either before a command has run. A shell it does not know
//! falls back to the heuristic and says so.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use forge_terminal::{Terminal, TerminalConfig};

/// Whether this machine has `shell` at all: the Linux CI runner has no zsh,
/// and a test that cannot start its shell is not a failure of the marks.
fn installed(shell: &str) -> bool {
    std::path::Path::new(shell).exists()
}

/// A terminal on `shell`, with a home of its own so the person's rc does not
/// decide what the test sees.
fn terminal_for(shell: &str) -> (Terminal, tempfile::TempDir) {
    let home = tempfile::tempdir().expect("a temp home");
    let mut env = HashMap::new();
    env.insert("HOME".to_owned(), home.path().display().to_string());
    env.insert("PATH".to_owned(), "/usr/bin:/bin:/usr/sbin:/sbin".to_owned());
    let config = TerminalConfig {
        shell: shell.to_owned(),
        cwd: home.path().to_path_buf(),
        env,
        cols: 80,
        rows: 24,
        scrollback_lines: 500,
    };
    let terminal = Terminal::spawn(&config).expect("a terminal");
    (terminal, home)
}

/// The output until it has been quiet for `quiet`, or `limit` passes.
fn collect(
    mut output: tokio::sync::broadcast::Receiver<forge_terminal::OutputChunk>,
    quiet: Duration,
    limit: Duration,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    let start = Instant::now();
    let mut last = Instant::now();
    while start.elapsed() < limit {
        match output.try_recv() {
            Ok(chunk) => {
                bytes.extend_from_slice(&chunk.bytes);
                last = Instant::now();
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                if !bytes.is_empty() && last.elapsed() >= quiet {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
    bytes
}

/// What the terminal has drawn so far, once it is quiet.
fn settle(terminal: &Terminal) -> Vec<u8> {
    collect(terminal.subscribe(), Duration::from_millis(300), Duration::from_secs(10))
}

/// The OSC 133 payloads in `bytes`, in order (A, B, C, D or D;N).
fn marks(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut payloads = Vec::new();
    let mut rest = text.as_ref();
    while let Some(at) = rest.find("\u{1b}]133;") {
        let after = &rest[at + 6..];
        let end = after.find('\u{7}').or_else(|| after.find("\u{1b}\\"));
        let Some(end) = end else { break };
        payloads.push(after[..end].to_owned());
        rest = &after[end..];
    }
    payloads
}

/// The marks one command adds: the command's own start and end, then the next
/// prompt. Anything else at the front - the prompt that was already drawn - is
/// not this command's.
fn command_marks(terminal: &Terminal, command: &[u8]) -> Vec<String> {
    let mut output = terminal.subscribe();
    // Drop whatever is pending, so the collection starts at the command; the
    // receiver is held across the write, so nothing the command emits is lost.
    while output.try_recv().is_ok() {}
    terminal.write_bytes(command).expect("a write");
    marks(&collect(output, Duration::from_millis(250), Duration::from_secs(10)))
}

/// A prompt is drawn as A then B, and no command is claimed before one runs.
fn assert_prompt_only(bytes: &[u8], shell: &str) {
    let found = marks(bytes);
    assert!(found.iter().any(|mark| mark == "A"), "{shell}: a prompt start: {found:?}");
    assert!(found.iter().any(|mark| mark == "B"), "{shell}: a prompt end: {found:?}");
    assert!(!found.iter().any(|mark| mark == "C"), "{shell}: no command ran: {found:?}");
    assert!(
        !found.iter().any(|mark| mark.starts_with('D')),
        "{shell}: no command ended: {found:?}"
    );
}

/// A failing command reports its own status, not zero.
fn assert_exit(found: &[String], code: &str, shell: &str) {
    let start = found.iter().position(|mark| mark == "C");
    let end = found.iter().position(|mark| mark == &format!("D;{code}"));
    assert!(start.is_some(), "{shell}: the command started: {found:?}");
    assert!(end.is_some(), "{shell}: the command ended {code}: {found:?}");
    assert!(start < end, "{shell}: start before end: {found:?}");
}

#[test]
fn zsh_reports_the_command_and_its_exit() {
    if !installed("/bin/zsh") {
        eprintln!("skipping the zsh marks: /bin/zsh is not installed here");
        return;
    }
    let (terminal, _home) = terminal_for("/bin/zsh");
    assert_prompt_only(&settle(&terminal), "zsh");
    assert_exit(&command_marks(&terminal, b"false\r"), "1", "zsh");
    assert_exit(&command_marks(&terminal, b"(exit 7)\r"), "7", "zsh");
    let interrupted = {
        let output = terminal.subscribe();
        terminal.write_bytes(b"sleep 5\r").expect("a write");
        std::thread::sleep(Duration::from_millis(300));
        terminal.write_bytes(&[3]).expect("an interrupt");
        marks(&collect(output, Duration::from_millis(250), Duration::from_secs(10)))
    };
    assert!(interrupted.iter().any(|mark| mark == "D;130"), "zsh: interrupt: {interrupted:?}");
    terminal.close();
}

#[test]
fn bash_reports_the_command_and_its_exit() {
    if !installed("/bin/bash") {
        eprintln!("skipping the bash marks: /bin/bash is not installed here");
        return;
    }
    let (terminal, _home) = terminal_for("/bin/bash");
    assert_prompt_only(&settle(&terminal), "bash");
    assert_exit(&command_marks(&terminal, b"false\r"), "1", "bash");
    // A subshell gets no DEBUG trap in bash 3.2, so this is a plain command.
    assert_exit(&command_marks(&terminal, b"sh -c 'exit 7'\r"), "7", "bash");
    let interrupted = {
        let output = terminal.subscribe();
        terminal.write_bytes(b"sleep 5\r").expect("a write");
        std::thread::sleep(Duration::from_millis(300));
        terminal.write_bytes(&[3]).expect("an interrupt");
        marks(&collect(output, Duration::from_millis(250), Duration::from_secs(10)))
    };
    assert!(interrupted.iter().any(|mark| mark == "D;130"), "bash: interrupt: {interrupted:?}");
    terminal.close();
}

/// A shell with no integration falls back: the command runs and the prompt
/// comes back, and the state says the answer is the heuristic.
#[test]
fn an_unknown_shell_falls_back_to_the_heuristic() {
    let (terminal, _home) = terminal_for("/bin/sh");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !terminal.screen().text.contains('$') {
        std::thread::sleep(Duration::from_millis(20));
    }
    terminal.write_bytes(b"echo FALLBACK\r").expect("a write");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut settled = false;
    while Instant::now() < deadline {
        let state = terminal.state();
        if terminal.screen().text.contains("FALLBACK")
            && !state.running
            && state
                .input_expected
                .as_ref()
                .is_some_and(|expected| expected.kind == forge_terminal::InputKind::Prompt)
        {
            settled = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(settled, "the fallback read the prompt: {:?}", terminal.state());
    assert_eq!(terminal.state().source, forge_terminal::Source::Heuristic);
    terminal.close();
}
