//! What the terminal is doing, as the agent reads it.
//!
//! **Two answers, and the state says which one it is.** With shell
//! integration the marks are exact: a `C` is a command running, a `D;N` is
//! that command finished with `N`. Without it - a shell that has no
//! integration, or a full-screen program that emits no marks - the classifier
//! falls back to "quiet at a prompt-shaped line" and says `heuristic`, so the
//! model knows what it has rather than trusting a guess as a fact.

use serde::{Deserialize, Serialize};

use crate::osc::Marks;

/// Where a state answer came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The shell's OSC 133 marks.
    Exact,
    /// The screen's shape, because the shell emitted no marks.
    Heuristic,
}

/// What kind of input the terminal is waiting for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    /// The terminal has ECHO off: a program is reading a secret.
    Password,
    /// The last line asks a yes/no question.
    Confirm,
    /// The screen is quiet at a prompt-shaped line.
    Prompt,
}

/// One input the terminal is waiting for, with the evidence behind it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputExpected {
    pub kind: InputKind,
    /// One sentence, so the model can disagree with the classifier.
    pub evidence: String,
}

/// A command's exit, as the shell or the process reported it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// What the terminal is doing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// Whether a command is running.
    pub running: bool,
    /// The input the terminal is waiting for, when it is waiting for one.
    pub input_expected: Option<InputExpected>,
    /// The last command's exit, when it has one.
    pub exit: Option<Exit>,
    /// Whether the answer came from the marks or the fallback.
    pub source: Source,
}

/// Everything the classifier reads.
#[derive(Clone, Copy)]
pub struct ClassifyInput<'a> {
    pub marks: Marks,
    /// Whether the PTY has ECHO on. Off means a secret is being read.
    pub echo: bool,
    /// Whether the screen has been quiet for the quiet window.
    pub quiet: bool,
    /// The last non-empty line on the screen.
    pub last_line: &'a str,
}

/// Classify the terminal's state.
pub fn classify(input: ClassifyInput<'_>) -> State {
    // **ECHO off is a password only while a command runs.** zsh's line editor
    // turns ECHO off at every prompt, so an idle prompt with ECHO off is the
    // shell waiting for the person, not a program reading a secret. With the
    // marks, a `C` with no `D` is the command that owns the ECHO-off state;
    // without them the flag is the best signal there is.
    let password = !input.echo && (!input.marks.seen || input.marks.running);
    let input_expected = if password {
        Some(InputExpected {
            kind: InputKind::Password,
            evidence: "the terminal has ECHO off while a command runs, so the program is reading \
                       a secret"
                .to_owned(),
        })
    } else if let Some(evidence) = confirm_evidence(input.last_line) {
        Some(InputExpected { kind: InputKind::Confirm, evidence })
    } else if input.quiet && is_prompt_shaped(input.last_line) {
        Some(InputExpected {
            kind: InputKind::Prompt,
            evidence: format!(
                "the screen is quiet at a prompt-shaped line: {}",
                input.last_line.trim()
            ),
        })
    } else {
        None
    };

    if input.marks.seen {
        let exit = if input.marks.running {
            None
        } else {
            input.marks.last_exit.map(|code| Exit { code: Some(code), signal: None })
        };
        return State { running: input.marks.running, input_expected, exit, source: Source::Exact };
    }

    // The fallback: a quiet prompt-shaped line is a shell waiting for the
    // person; anything else is a command still working.
    let at_prompt = input.quiet && is_prompt_shaped(input.last_line);
    State { running: !at_prompt, input_expected, exit: None, source: Source::Heuristic }
}

/// The evidence a yes/no question is on the last line, when it is.
fn confirm_evidence(line: &str) -> Option<String> {
    let trimmed = line.trim_end();
    let lower = trimmed.to_ascii_lowercase();
    let asks = lower.ends_with("[y/n]")
        || lower.ends_with("[y/n]?")
        || lower.ends_with("(y/n)")
        || lower.ends_with("(yes/no)")
        || lower.ends_with("continue?")
        || lower.ends_with("are you sure?")
        || lower.ends_with("proceed?")
        || lower.ends_with("overwrite?");
    asks.then(|| format!("the last line asks a yes/no question: {}", trimmed.trim()))
}

/// Whether a line looks like a shell prompt waiting for input.
fn is_prompt_shaped(line: &str) -> bool {
    let trimmed = line.trim_end();
    if trimmed.is_empty() {
        return false;
    }
    let last = trimmed.chars().last().unwrap_or(' ');
    if !matches!(last, '$' | '#' | '%' | '>' | '❯' | 'λ') {
        return false;
    }
    // A prompt is a short line ending in its sigil, not a line of prose that
    // happens to end in `>`.
    trimmed.chars().count() <= 200
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::osc::Mark;

    fn marks(seen: bool, running: bool, last_exit: Option<i32>) -> Marks {
        Marks { seen, running, last_exit, last: None }
    }

    fn input(marks: Marks, echo: bool, quiet: bool, last_line: &str) -> ClassifyInput<'_> {
        ClassifyInput { marks, echo, quiet, last_line }
    }

    /// **ECHO off while a command runs is a password.** The program is
    /// reading a secret, and the evidence names the termios flag rather than
    /// guessing from the text.
    #[test]
    fn echo_off_while_a_command_runs_is_a_password() {
        let state = classify(input(marks(true, true, None), false, true, "Password: "));
        let expected = state.input_expected.expect("a password is expected");
        assert_eq!(expected.kind, InputKind::Password);
        assert!(expected.evidence.contains("ECHO off"), "{}", expected.evidence);
    }

    /// **ECHO off at an idle prompt is not a password.** zsh's line editor
    /// turns ECHO off at every prompt, so the flag alone would call every
    /// idle shell a secret read; the marks say no command is running.
    #[test]
    fn echo_off_at_an_idle_prompt_is_not_a_password() {
        let state = classify(input(marks(true, false, Some(0)), false, true, "ved@host:~ $ "));
        assert_ne!(
            state.input_expected.map(|expected| expected.kind),
            Some(InputKind::Password),
            "an idle prompt is the shell waiting, not a secret",
        );
    }

    /// A yes/no line is a confirm, and the evidence quotes the line.
    #[test]
    fn a_yes_no_line_is_a_confirm() {
        let state =
            classify(input(marks(false, false, None), true, true, "Overwrite file? [y/N] "));
        let expected = state.input_expected.expect("a confirm is expected");
        assert_eq!(expected.kind, InputKind::Confirm);
        assert!(expected.evidence.contains("[y/N]"), "{}", expected.evidence);
    }

    /// A quiet prompt-shaped line is a prompt, and the fallback says
    /// `heuristic`.
    #[test]
    fn a_quiet_prompt_is_a_prompt_and_says_heuristic() {
        let state = classify(input(marks(false, false, None), true, true, "ved@host:~/p $ "));
        assert_eq!(state.source, Source::Heuristic);
        assert!(!state.running, "a quiet prompt is not a running command");
        assert_eq!(state.input_expected.expect("a prompt").kind, InputKind::Prompt);
    }

    /// **The marks are exact.** A `C` is running, a `D;7` is a failure, and
    /// the source says `exact` so the model trusts it.
    #[test]
    fn the_marks_are_exact() {
        let running = classify(input(marks(true, true, None), true, false, ""));
        assert_eq!(running.source, Source::Exact);
        assert!(running.running);
        assert_eq!(running.exit, None);

        let done = classify(input(marks(true, false, Some(7)), true, true, "$ "));
        assert_eq!(done.source, Source::Exact);
        assert!(!done.running);
        assert_eq!(done.exit, Some(Exit { code: Some(7), signal: None }));
    }

    /// A line of prose ending in `>` is not a prompt: the fallback must not
    /// read a quoted email as a shell waiting for input.
    #[test]
    fn a_long_line_ending_in_a_sigil_is_not_a_prompt() {
        let prose = format!("{} >", "a long sentence that happens to end in a bracket ".repeat(6));
        let state = classify(input(marks(false, false, None), true, true, &prose));
        assert!(state.input_expected.is_none(), "prose is not a prompt");
        assert!(state.running, "and the fallback reads it as work in progress");
    }

    /// The last mark is carried for a reader that wants it.
    #[test]
    fn the_last_mark_is_carried() {
        let mut m = marks(true, false, Some(0));
        m.last = Some(Mark::CommandEnd(Some(0)));
        assert_eq!(m.last, Some(Mark::CommandEnd(Some(0))));
    }
}
