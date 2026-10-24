//! The handoff brief: forge's own ground truth, and the handing-over
//! session's judgment.
//!
//! The ground truth is derived at swap time from the transcript, git and the
//! board, so every path, command and number in it is read rather than
//! remembered. The judgment is the old session's reply to forge's own prompt,
//! captured before the swap while its context still holds the work.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use forge_agent::client::SessionLaunchSettings;
use forge_primitives::{
    ContentBlock, HandoffBrief, HandoffCycle, HandoffGitState, HandoffReason, HandoffRecord,
    HandoffSection, Message, Task, TaskStatus,
};

use crate::SessionSlot;
use crate::protocol::{Command, NoticeSeverity, SessionUpdate};

/// How many files, commands and prompt lines the brief names. Past this it
/// starts re-summarising the session, which is the failure it exists to
/// avoid.
const FILE_CAP: usize = 10;
const COMMAND_CAP: usize = 10;
const DIRTY_CAP: usize = 20;
const CONSTRAINT_CAP: usize = 10;
const CONSTRAINT_LINE_CAP: usize = 300;
const PROMPT_LINE_CAP: usize = 600;
const MEASURED_LINE_CAP: usize = 200;
/// The judgment half's ceiling. Long enough for the five headings written
/// properly, short enough that the brief stays inside its 3-4k-token budget.
const JUDGMENT_CAP: usize = 9_000;

/// The parts of the judgment half forge asks for, in order.
pub(crate) const JUDGMENT_HEADINGS: [&str; 5] =
    ["Now", "Tried and failed", "Do not", "Next", "Done means"];

/// What the seat is, so the brief can say which charter it belongs to
/// without repeating a charter the launch already carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Role {
    Lead,
    Worker { charter_opening: Option<String> },
}

/// The seat a brief is written for.
pub(crate) struct HandoffSeat<'a> {
    pub slot: &'a SessionSlot,
    pub from: &'a str,
    pub cwd: &'a Path,
    pub transcript_path: &'a Path,
    pub account: Option<String>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    pub role: Role,
}

/// The ground truth's sections plus the cycle they end.
pub(crate) struct GroundTruth {
    pub sections: Vec<HandoffSection>,
    pub cycle: HandoffCycle,
    /// Every path the handover read, which the successor's re-reads are
    /// measured against. Store-only: no view draws it.
    pub reads: Vec<String>,
}

/// How many of the successor's turns are measured.
pub(crate) const SUCCESSOR_TURNS: u32 = 10;

/// The predecessor's read set the successor is measured against is bounded:
/// a record is not a transcript, and the count only needs the files the two
/// sessions plausibly meet in.
const READ_SET_CAP: usize = 200;

/// Build the brief's first half. The transcript read blocks, so it runs off
/// the actor; the git probe waits on child processes and stays async.
pub(crate) async fn ground_truth(
    config_dir: &Path,
    seat: &HandoffSeat<'_>,
    board: &[Task],
) -> GroundTruth {
    let read = {
        let config_dir = config_dir.to_path_buf();
        let session_id = seat.from.to_owned();
        let cwd = seat.cwd.to_path_buf();
        // The read is the whole transcript; the brief needs its first ask,
        // its newest asks and the calls between, which only a pass over the
        // file can answer.
        tokio::task::spawn_blocking(move || {
            forge_agent::session_history(&config_dir, &session_id, &cwd.to_string_lossy())
        })
        .await
    };
    let messages = read.map(|history| history.messages).unwrap_or_default();
    let git = forge_agent::env::git_state::probe(seat.cwd).await;
    assemble(seat, &messages, &git, board)
}

/// The deterministic half, pure over its inputs: what the tests drive.
pub(crate) fn assemble(
    seat: &HandoffSeat<'_>,
    messages: &[Message],
    git: &HandoffGitState,
    board: &[Task],
) -> GroundTruth {
    let asks = user_prompts(messages);
    GroundTruth {
        sections: vec![
            seat_section(seat, git),
            role_section(&seat.role),
            ask_section(&asks),
            constraint_section(&asks),
            files_section(messages, git),
            verify_section(messages),
            board_section(seat.slot, board),
            record_section(seat),
        ],
        cycle: cycle(messages, asks.len()),
        reads: read_set(messages),
    }
}

/// Every path the session read, in transcript order, deduped and capped.
fn read_set(messages: &[Message]) -> Vec<String> {
    let mut reads: Vec<String> = Vec::new();
    for (_, name, input) in tool_uses(messages) {
        if !matches!(name, "Read" | "NotebookRead") {
            continue;
        }
        let Some(path) = input.get("file_path").or_else(|| input.get("notebook_path")) else {
            continue;
        };
        let Some(path) = path.as_str() else { continue };
        if !reads.iter().any(|seen| seen == path) {
            reads.push(path.to_owned());
        }
        if reads.len() >= READ_SET_CAP {
            break;
        }
    }
    reads
}

/// How the successor's first turns went, from its own transcript.
///
/// The first user row is the brief forge gave it, not a turn of its own, so
/// it is skipped - and it must be, because the judgment half can carry a
/// "do not" rule that would otherwise read as a correction. A reply the
/// user typed later is measured for correction cues; the count is a proxy,
/// and the anchor is that it only ever counts words a person wrote.
pub(crate) fn successor_counts(
    messages: &[Message],
    predecessor_reads: &[String],
) -> forge_primitives::HandoffSuccessor {
    let mut turns = 0;
    let mut reads: Vec<String> = Vec::new();
    let mut questions = 0;
    let mut corrections = 0;
    for message in messages {
        match message {
            Message::User { message: envelope, synthetic, .. } if !*synthetic => {
                let Some(text) = text_of(&envelope.content) else { continue };
                if turns == 0 && text.starts_with("HANDOFF - ") {
                    continue;
                }
                // The window closes when an eleventh prompt arrives: that
                // turn is the successor's own work, not the brief's doing,
                // and the tenth turn's own frames are already counted.
                if turns >= SUCCESSOR_TURNS {
                    break;
                }
                turns += 1;
                if turns > 1 && states_a_correction(&text) {
                    corrections += 1;
                }
            }
            Message::Assistant { message: envelope, .. } => {
                for block in &envelope.content {
                    let ContentBlock::ToolUse { name, input, .. } = block else { continue };
                    match name.as_str() {
                        "AskUserQuestion" => questions += 1,
                        "Read" | "NotebookRead" => {
                            let path = input
                                .get("file_path")
                                .or_else(|| input.get("notebook_path"))
                                .and_then(serde_json::Value::as_str);
                            if let Some(path) = path
                                && predecessor_reads.iter().any(|read| read == path)
                                && !reads.iter().any(|seen| seen == path)
                            {
                                reads.push(path.to_owned());
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    forge_primitives::HandoffSuccessor {
        turns,
        reread_files: u32::try_from(reads.len()).unwrap_or(u32::MAX),
        questions,
        corrections,
    }
}

/// Whether a user prompt reads as a correction of what came before.
fn states_a_correction(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "actually",
        "i said",
        "as i said",
        "not what i asked",
        "that is wrong",
        "that's wrong",
        "you missed",
        "you forgot",
    ]
    .iter()
    .any(|cue| lower.contains(cue))
}

fn seat_section(seat: &HandoffSeat<'_>, git: &HandoffGitState) -> HandoffSection {
    let mut lines = vec![seat.slot.display(), format!("cwd {}", seat.cwd.display())];
    match (git.branch.as_deref(), git.head.as_deref()) {
        (Some(branch), Some(head)) => lines.push(format!("branch {branch} @ {head}")),
        (Some(branch), None) => lines.push(format!("branch {branch}")),
        (None, _) => lines.push("not a git repository".to_owned()),
    }
    if let Some(account) = &seat.account {
        lines.push(format!("account {account}"));
    }
    if let Some(model) = &seat.model {
        lines.push(format!("model {model}"));
    }
    if let Some(mode) = &seat.permission_mode {
        lines.push(format!("mode {mode}"));
    }
    HandoffSection { label: "seat".to_owned(), lines }
}

fn role_section(role: &Role) -> HandoffSection {
    let lines = match role {
        Role::Lead => vec!["project lead".to_owned()],
        Role::Worker { charter_opening } => match charter_opening {
            Some(opening) => vec![format!("worker, charter opens: {opening}")],
            None => vec!["worker".to_owned()],
        },
    };
    HandoffSection { label: "role".to_owned(), lines }
}

fn ask_section(asks: &[(usize, String)]) -> HandoffSection {
    let mut lines = Vec::new();
    let recent = asks.len().saturating_sub(3);
    // The first ask is the session's intent; when it is already inside the
    // recent window it is not repeated.
    if recent > 0
        && let Some((_, first)) = asks.first()
    {
        lines.push(format!("first: {}", clipped(first, PROMPT_LINE_CAP)));
    }
    for (ordinal, text) in &asks[recent..] {
        lines.push(format!("#{ordinal}: {}", clipped(text, PROMPT_LINE_CAP)));
    }
    HandoffSection { label: "ask".to_owned(), lines }
}

/// The rules still binding, quoted. A cue word is the pattern and a heading
/// the user wrote is the other, so a constraint is picked out of the prompt
/// that stated it rather than paraphrased from it.
fn constraint_section(asks: &[(usize, String)]) -> HandoffSection {
    let mut found: Vec<String> = Vec::new();
    for (ordinal, text) in asks.iter().rev() {
        for line in text.lines().rev() {
            let line = line.trim();
            if line.is_empty() || !states_a_rule(line) {
                continue;
            }
            let quoted =
                format!("\"{}\" (user prompt {ordinal})", clipped(line, CONSTRAINT_LINE_CAP));
            if !found.contains(&quoted) {
                found.push(quoted);
            }
            if found.len() >= CONSTRAINT_CAP {
                found.reverse();
                return HandoffSection { label: "constraints".to_owned(), lines: found };
            }
        }
    }
    found.reverse();
    HandoffSection { label: "constraints".to_owned(), lines: found }
}

/// Whether a line states a rule rather than a task. Deliberately narrow:
/// missing a line leaves the successor the transcript, while a broad match
/// fills the brief with prose.
fn states_a_rule(line: &str) -> bool {
    let lower = line.to_lowercase();
    ["must", "never", "do not", "don't", "no exceptions", "hard rule", "forbidden", "required"]
        .iter()
        .any(|cue| lower.contains(cue))
        || lower.starts_with("always ")
}

fn files_section(messages: &[Message], git: &HandoffGitState) -> HandoffSection {
    let mut changed: Vec<String> = Vec::new();
    let mut read_paths: Vec<String> = Vec::new();
    for (_, name, input) in tool_uses(messages) {
        let Some(path) = input.get("file_path").or_else(|| input.get("notebook_path")) else {
            continue;
        };
        let Some(path) = path.as_str() else { continue };
        let bucket = match name {
            "Edit" | "Write" | "NotebookEdit" | "MultiEdit" => &mut changed,
            "Read" | "NotebookRead" => &mut read_paths,
            _ => continue,
        };
        if !bucket.iter().any(|seen| seen == path) {
            bucket.push(path.to_owned());
        }
    }
    let mut lines = Vec::new();
    if !changed.is_empty() {
        lines.push(format!("changed: {}", tail(&changed, FILE_CAP).join(", ")));
    }
    if !read_paths.is_empty() {
        lines.push(format!("read: {}", tail(&read_paths, FILE_CAP).join(", ")));
    }
    if git.dirty.is_empty() {
        lines.push("uncommitted: none".to_owned());
    } else {
        let shown = tail(&git.dirty, DIRTY_CAP);
        lines.push(format!("uncommitted: {} paths", git.dirty.len()));
        lines.extend(shown);
    }
    if !git.diff.is_empty() {
        lines.push("diff:".to_owned());
        lines.extend(git.diff.iter().cloned());
    }
    HandoffSection { label: "files".to_owned(), lines }
}

fn verify_section(messages: &[Message]) -> HandoffSection {
    let mut lines = Vec::new();
    let results = tool_results(messages);
    let calls = bash_calls(messages);
    // The commands the session ran, oldest first, deduped: a command run
    // three times is one line, and the order says what was tried before what.
    let mut recent: Vec<&str> = Vec::new();
    for (_, command) in &calls {
        if !recent.contains(command) {
            recent.push(command);
        }
    }
    for command in tail(&recent, COMMAND_CAP) {
        lines.push(format!("ran: {}", clipped(command, MEASURED_LINE_CAP)));
    }
    let gate = calls
        .into_iter()
        .rev()
        .find(|(_, command)| is_a_gate(command))
        .map(|(id, command)| (id, command.to_owned()));
    if let Some((id, command)) = gate {
        let outcome = results.get(&id);
        let failed = outcome.is_some_and(|(is_error, _)| *is_error);
        let verdict = if failed { "failed" } else { "ok" };
        lines.push(format!("gate: {} -> {verdict}", clipped(&command, MEASURED_LINE_CAP)));
        if failed {
            let failing = outcome
                .and_then(|(_, text)| failing_line(text))
                .unwrap_or_else(|| "see the transcript".to_owned());
            lines.push(format!("failing: {failing}"));
        }
    }
    if let Some(measured) = measured_line(messages) {
        lines.push(format!("measured: {}", clipped(&measured, MEASURED_LINE_CAP)));
    }
    HandoffSection { label: "verify".to_owned(), lines }
}

fn board_section(slot: &SessionSlot, board: &[Task]) -> HandoffSection {
    let mut lines = Vec::new();
    let mut refs: Vec<String> = Vec::new();
    for task in board.iter().filter(|task| task.owner.as_ref() == Some(slot)) {
        lines.push(format!("{} ({})", task.subject, status_name(task.status)));
        for link in &task.links {
            let entry = format!("{}: {}", link_kind(link.kind), link.target);
            if !refs.contains(&entry) {
                refs.push(entry);
            }
        }
    }
    if lines.is_empty() {
        lines.push("no rows owned".to_owned());
    }
    if !refs.is_empty() {
        lines.push("links:".to_owned());
        lines.extend(tail(&refs, FILE_CAP));
    }
    HandoffSection { label: "board".to_owned(), lines }
}

fn record_section(seat: &HandoffSeat<'_>) -> HandoffSection {
    HandoffSection {
        label: "record".to_owned(),
        lines: vec![
            seat.transcript_path.display().to_string(),
            format!("session {}", seat.from),
            "anything this brief does not carry is in that transcript, not guessed".to_owned(),
        ],
    }
}

fn status_name(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::InProgress => "in progress",
        TaskStatus::Waiting => "waiting",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Canceled => "canceled",
    }
}

fn link_kind(kind: forge_primitives::tasks::LinkKind) -> &'static str {
    use forge_primitives::tasks::LinkKind;
    match kind {
        LinkKind::Spec => "spec",
        LinkKind::Plan => "plan",
        LinkKind::Issue => "issue",
        LinkKind::Pr => "pr",
        LinkKind::Branch => "branch",
        LinkKind::Path => "path",
        LinkKind::Other => "link",
    }
}

/// The cycle a handoff ends: one request per assistant row that carries a
/// usage block, one turn per real user prompt, and the prompt tokens the
/// model read.
fn cycle(messages: &[Message], turns: usize) -> HandoffCycle {
    let mut requests = 0;
    let mut prompt_tokens = 0;
    for message in messages {
        let Message::Assistant { message: envelope, .. } = message else { continue };
        let Some(usage) = &envelope.usage else { continue };
        requests += 1;
        prompt_tokens +=
            usage.input_tokens + usage.cache_creation_input_tokens + usage.cache_read_input_tokens;
    }
    HandoffCycle { requests, turns: u32::try_from(turns).unwrap_or(u32::MAX), prompt_tokens }
}

/// Every real user prompt, ordinal and text, in transcript order. A
/// synthetic row is the harness speaking, and a row with no text is a tool
/// result rather than something a person or a peer said.
fn user_prompts(messages: &[Message]) -> Vec<(usize, String)> {
    let mut prompts = Vec::new();
    for message in messages {
        let Message::User { message: envelope, synthetic, .. } = message else { continue };
        if *synthetic {
            continue;
        }
        let Some(text) = text_of(&envelope.content) else { continue };
        prompts.push((prompts.len() + 1, text));
    }
    prompts
}

/// Every tool call's id, name and input, in transcript order.
fn tool_uses(messages: &[Message]) -> impl Iterator<Item = (&str, &str, &serde_json::Value)> {
    messages.iter().flat_map(|message| {
        let Message::Assistant { message: envelope, .. } = message else {
            return Vec::new().into_iter();
        };
        envelope
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, input, .. } => {
                    Some((id.as_str(), name.as_str(), input))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .into_iter()
    })
}

/// Bash calls with their tool-use id and command, in order.
fn bash_calls(messages: &[Message]) -> Vec<(String, &str)> {
    tool_uses(messages)
        .filter(|(_, name, input)| {
            *name == "Bash" && input.get("command").and_then(serde_json::Value::as_str).is_some()
        })
        .filter_map(|(id, _, input)| {
            let command = input.get("command")?.as_str()?;
            Some((id.to_owned(), command))
        })
        .collect()
}

/// Tool results by the call they answer: whether it failed, and its text.
fn tool_results(messages: &[Message]) -> std::collections::HashMap<String, (bool, String)> {
    let mut results = std::collections::HashMap::new();
    for message in messages {
        let Message::User { message: envelope, .. } = message else { continue };
        for block in &envelope.content {
            let ContentBlock::ToolResult { tool_use_id, content, is_error, .. } = block else {
                continue;
            };
            results.insert(tool_use_id.clone(), (*is_error, result_text(content)));
        }
    }
    results
}

fn result_text(content: &serde_json::Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    if let Some(blocks) = content.as_array() {
        let joined: Vec<&str> = blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str))
            .collect();
        if !joined.is_empty() {
            return joined.join("\n");
        }
    }
    content.to_string()
}

/// Whether a command is a gate: the checks this project treats as the bar.
fn is_a_gate(command: &str) -> bool {
    [
        "just check",
        "just check-feature-configs",
        "just check-delta",
        "cargo nextest",
        "cargo test",
        "cargo clippy",
        "npm test",
        "npm run test",
        "pnpm test",
        "bun test",
        "pytest",
        "make test",
    ]
    .iter()
    .any(|gate| command.contains(gate))
}

/// The first line of a failed run's output that names the failure.
fn failing_line(output: &str) -> Option<String> {
    output
        .lines()
        .map(str::trim)
        .find(|line| {
            line.contains("FAILED")
                || line.contains("FAIL [")
                || line.starts_with("error:")
                || line.contains(" test result: failed")
        })
        .map(|line| clipped(line, MEASURED_LINE_CAP))
}

/// The last line the session stated a number in, which is the shape
/// "measured" takes. Narrow on purpose: a unit, not a bare digit.
fn measured_line(messages: &[Message]) -> Option<String> {
    for message in messages.iter().rev() {
        let Message::Assistant { message: envelope, .. } = message else { continue };
        for block in envelope.content.iter().rev() {
            let ContentBlock::Text { text, .. } = block else { continue };
            for line in text.lines().rev() {
                let line = line.trim();
                if states_a_number(line) {
                    return Some(line.to_owned());
                }
            }
        }
    }
    None
}

/// Whether a line states a number in a unit: "1.2 s", "512 MB", "85%".
/// A bare digit is not enough - a line of prose is not a measurement.
fn states_a_number(line: &str) -> bool {
    const UNITS: [&str; 11] =
        ["ms", "mb", "mib", "gb", "gib", "kb", "s", "%", "tokens", "tok", "req/s"];
    let lower = line.to_lowercase();
    UNITS.iter().any(|unit| {
        let mut from = 0;
        while let Some(at) = lower[from..].find(unit) {
            let at = from + at;
            let before = lower[..at].trim_end().chars().next_back();
            let after = lower[at + unit.len()..].chars().next();
            let ends_here = after.is_none_or(|next| !next.is_alphanumeric());
            if ends_here && before.is_some_and(|before| before.is_ascii_digit()) {
                return true;
            }
            from = at + unit.len();
        }
        false
    })
}

/// The plain text of an assistant turn's blocks, when it holds any.
pub(crate) fn assistant_text(blocks: &[ContentBlock]) -> Option<String> {
    text_of(blocks)
}

/// The plain text of a block list, when it holds any.
fn text_of(blocks: &[ContentBlock]) -> Option<String> {
    let parts: Vec<&str> = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let text = parts.join("\n");
    (!text.trim().is_empty()).then_some(text)
}

/// The last `cap` entries.
fn tail<T: Clone>(items: &[T], cap: usize) -> Vec<T> {
    items[items.len().saturating_sub(cap)..].to_vec()
}

fn clipped(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_owned();
    }
    let cut: String = text.chars().take(cap).collect();
    format!("{cut}...")
}

/// Parse the handing-over session's reply into the judgment half.
///
/// A heading the model wrote differently is not dropped: its lines join the
/// section before it, and a reply with no heading at all is one section. The
/// row draws what the successor was given, not an idealised version of it.
pub(crate) fn parse_judgment(raw: &str) -> Vec<HandoffSection> {
    let mut sections: Vec<HandoffSection> = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if let Some((heading, rest)) = judgment_heading(trimmed) {
            sections
                .push(HandoffSection { label: heading.to_owned(), lines: vec![rest.to_owned()] });
            continue;
        }
        match sections.last_mut() {
            Some(section) => section.lines.push(trimmed.to_owned()),
            None if !trimmed.is_empty() => {
                sections.push(HandoffSection {
                    label: "judgment".to_owned(),
                    lines: vec![trimmed.to_owned()],
                });
            }
            None => {}
        }
    }
    for section in &mut sections {
        section.lines.retain(|line| !line.is_empty());
    }
    cap_judgment(sections)
}

/// The heading a line opens, and the rest of the line after its colon.
fn judgment_heading(line: &str) -> Option<(&'static str, &str)> {
    let stripped = line.trim_start_matches(['#', '*', '-', ' ']);
    for heading in JUDGMENT_HEADINGS {
        let Some(rest) = stripped.strip_prefix(heading) else { continue };
        let Some(rest) = rest.strip_prefix(':') else { continue };
        return Some((heading, rest.trim()));
    }
    None
}

/// Keep the whole half under its ceiling. A cut is stated - the brief never
/// silently loses the tail of what the old session wrote.
fn cap_judgment(mut sections: Vec<HandoffSection>) -> Vec<HandoffSection> {
    let mut budget = JUDGMENT_CAP;
    let mut kept: Vec<HandoffSection> = Vec::new();
    let mut cut = false;
    for section in sections.drain(..) {
        let mut lines = Vec::new();
        for line in section.lines {
            let cost = line.chars().count() + 1;
            if cost > budget {
                cut = true;
                break;
            }
            budget -= cost;
            lines.push(line);
        }
        if !lines.is_empty() {
            kept.push(HandoffSection { label: section.label, lines });
        }
        if cut {
            break;
        }
    }
    if cut {
        // The cut is stated even when nothing fit: a half that silently
        // vanished would read as a reply that said nothing.
        match kept.last_mut() {
            Some(section) => section
                .lines
                .push("[the rest of this reply is in the transcript named above]".to_owned()),
            None => kept.push(HandoffSection {
                label: "judgment".to_owned(),
                lines: vec![
                    "[the reply was longer than the brief allows; it is in the transcript \
                     named above]"
                        .to_owned(),
                ],
            }),
        }
    }
    kept
}

/// The brief as the successor receives it.
pub(crate) fn brief_text(pair: &str, brief: &forge_primitives::HandoffBrief) -> String {
    let mut out = format!("HANDOFF - {pair}\n");
    out.push_str("\nPART 1: GROUND TRUTH (written by forge, not by a model)\n");
    render_sections(&mut out, &brief.ground_truth);
    out.push_str("\nPART 2: JUDGMENT (written by the session that is handing over)\n");
    render_sections(&mut out, &brief.judgment);
    out
}

fn render_sections(out: &mut String, sections: &[HandoffSection]) {
    use std::fmt::Write as _;
    for section in sections {
        for (at, line) in section.lines.iter().enumerate() {
            let _ = if at == 0 {
                writeln!(out, "{}: {line}", section.label)
            } else {
                writeln!(out, "    {line}")
            };
        }
        if section.lines.is_empty() {
            let _ = writeln!(out, "{}:", section.label);
        }
    }
}

/// The prompt forge sends for the judgment half. Shipped text: it names no
/// skill, so a fresh install gets the same brief.
pub(crate) const JUDGMENT_PROMPT: &str = "\
A handoff is starting for this session: its context is nearly full, so forge \
will replace it with a new session that continues the work. Write the judgment \
half of the brief the new session will be given, as the text of this turn and \
nothing else.\n\n\
Use exactly these five headings, each starting its own line:\n\n\
Now: what we are doing and why, in five sentences.\n\
Tried and failed: the dead ends, with the error that stopped each.\n\
Do not: the things that look right and are not.\n\
Next: the next concrete action, with the file or command it starts at.\n\
Done means: the observable that ends this task.\n\n\
Quote any constraint the user set verbatim. Name files, commands and numbers \
exactly. Do not restate what is on disk: forge's own half of the brief already \
carries the seat, the asks, the constraints, the files, the git state, the gate \
and the board. Keep the whole reply under 600 words.";

/// A prompt held because the seat was swapping its occupant.
#[derive(Debug, Clone)]
pub(crate) struct HeldPrompt {
    pub text: String,
    pub attachments: Vec<forge_primitives::ImageAttachment>,
    pub source: crate::protocol::PromptSource,
}

/// The handoff a session arrived by, while its first turns are measured.
#[derive(Debug, Clone)]
pub(crate) struct SuccessorWatch {
    /// The occupant the handoff named.
    pub to: String,
    /// Turns counted so far.
    pub turns: u32,
    /// What its predecessor had read.
    pub predecessor_reads: Vec<String>,
}

/// What a swap left for the occupant that continues: the text it is given,
/// and the record the seat draws. Held until that occupant connects.
#[derive(Debug, Clone)]
pub(crate) struct PendingBrief {
    /// The occupant this was written for. A connect under any other id is
    /// not the session the brief belongs to.
    pub to: String,
    pub text: String,
}

/// The fill reading a handoff fired on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillReading {
    pub percent: u8,
    pub total_tokens: u64,
    pub max_tokens: u64,
}

/// One handoff, decided and ready to run.
pub(crate) struct SwapRequest {
    pub slot: SessionSlot,
    pub from: String,
    pub to: String,
    pub reason: HandoffReason,
    pub fill: Option<FillReading>,
    pub compaction_count: u32,
    pub at_percent: u8,
    pub role: Role,
    pub account: Option<String>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    pub judgment: Vec<HandoffSection>,
    pub cwd: String,
    pub launch_settings: SessionLaunchSettings,
}

impl crate::Workspace {
    /// Hold the brief a handoff has written until its successor connects.
    pub(crate) fn set_pending_brief(&self, slot: &SessionSlot, brief: PendingBrief) {
        self.pending_handoffs.lock().insert(slot.clone(), brief);
    }

    /// Take the brief a swap left for `slot`, if one is waiting.
    pub(crate) fn take_pending_brief(&self, slot: &SessionSlot) -> Option<PendingBrief> {
        self.pending_handoffs.lock().remove(slot)
    }

    /// Forget a brief whose swap never reached its successor.
    pub(crate) fn clear_pending_brief(&self, slot: &SessionSlot) {
        self.pending_handoffs.lock().remove(slot);
    }

    /// Mark the seat as swapping its occupant: from the moment the brief is
    /// built until the successor connects, a prompt aimed at it is held
    /// rather than run by the session on its way out.
    pub(crate) fn begin_swap(&self, slot: &SessionSlot) {
        self.swapping.lock().insert(slot.clone());
    }

    /// Whether a swap is in flight on this seat.
    pub(crate) fn swap_in_progress(&self, slot: &SessionSlot) -> bool {
        self.swapping.lock().contains(slot)
    }

    /// Hold a prompt for the successor. `true` when a swap took it.
    pub(crate) fn hold_prompt(&self, slot: &SessionSlot, prompt: HeldPrompt) -> bool {
        if !self.swap_in_progress(slot) {
            return false;
        }
        self.held_prompts.lock().entry(slot.clone()).or_default().push(prompt);
        true
    }

    /// End the swap and take everything held for it, in arrival order.
    pub(crate) fn end_swap(&self, slot: &SessionSlot) -> Vec<HeldPrompt> {
        self.swapping.lock().remove(slot);
        self.held_prompts.lock().remove(slot).unwrap_or_default()
    }

    /// Write one handoff to the store, which is what `at_percent` is tuned
    /// from later. A store that cannot write is logged, never a reason to
    /// hold up the swap: the session is already handing over.
    pub(crate) fn record_handoff(&self, record: &HandoffRecord, predecessor_reads: &[String]) {
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else { return };
        if let Err(error) = crate::store::handoffs::put(db, record, predecessor_reads) {
            tracing::warn!(
                target: "forge_workspace::handoff",
                event_name = "handoff_record_failed",
                slot = %record.slot.display(),
                %error,
                "the handoff record could not be written; the swap itself is unaffected",
            );
        }
    }

    /// The seat's newest handoff record, read from the store.
    ///
    /// This is what a view attaching after the swap - or after a forge
    /// restart - draws the row from, where the transport holds no live copy.
    pub fn latest_handoff_for(&self, slot: &SessionSlot) -> Option<HandoffRecord> {
        let guard = self.db.lock();
        let db = guard.as_ref()?;
        match crate::store::handoffs::latest_for_slot(db, slot) {
            Ok(record) => record,
            Err(error) => {
                tracing::warn!(
                    target: "forge_workspace::handoff",
                    event_name = "handoff_record_unreadable",
                    slot = %slot.display(),
                    %error,
                    "the seat's newest handoff could not be read",
                );
                None
            }
        }
    }

    /// The handoff `occupant` arrived by, if it arrived by one.
    pub(crate) fn handoff_for_successor(
        &self,
        slot: &SessionSlot,
        occupant: &str,
    ) -> Option<(HandoffRecord, Vec<String>)> {
        let guard = self.db.lock();
        let db = guard.as_ref()?;
        match crate::store::handoffs::for_successor(db, slot, occupant) {
            Ok(found) => found,
            Err(error) => {
                tracing::warn!(
                    target: "forge_workspace::handoff",
                    event_name = "handoff_record_unreadable",
                    slot = %slot.display(),
                    %error,
                    "the handoff record could not be read; this seat's first turns go unmeasured",
                );
                None
            }
        }
    }

    /// Measure the successor's first turns off its own transcript and write
    /// them onto the record. Runs detached: the read is a whole transcript
    /// and the seat has work to do.
    pub(crate) fn measure_successor(
        self: &Arc<Self>,
        slot: SessionSlot,
        occupant: String,
        cwd: String,
        predecessor_reads: Vec<String>,
    ) {
        let workspace = Arc::clone(self);
        tokio::spawn(async move {
            let config_dir = workspace.config_dir().to_path_buf();
            let read_id = occupant.clone();
            let read = tokio::task::spawn_blocking(move || {
                forge_agent::session_history(&config_dir, &read_id, &cwd)
            })
            .await;
            let messages = read.map(|history| history.messages).unwrap_or_default();
            let counts = successor_counts(&messages, &predecessor_reads);
            let guard = workspace.db.lock();
            let Some(db) = guard.as_ref() else { return };
            if let Err(error) = crate::store::handoffs::set_successor(db, &slot, &occupant, counts)
            {
                tracing::warn!(
                    target: "forge_workspace::handoff",
                    event_name = "handoff_successor_measurement_failed",
                    slot = %slot.display(),
                    %error,
                    "the successor's first turns could not be written to the record",
                );
            }
        });
    }

    /// Run a handoff: build the ground truth, draw the row, and start the new
    /// session on the same order `/new` uses.
    ///
    /// The build reads the transcript and probes git, so it runs off the
    /// actor that decided it. The row goes out before the spawn, which is
    /// what puts it between the two conversations rather than below the new
    /// one's first turn.
    pub(crate) fn start_handoff(self: &Arc<Self>, request: SwapRequest) {
        let workspace = Arc::clone(self);
        tokio::spawn(async move {
            workspace.run_handoff(request).await;
        });
    }

    async fn run_handoff(self: Arc<Self>, request: SwapRequest) {
        // The gate opens before the read: building the brief takes seconds on
        // a long transcript, and a prompt arriving in that stretch is exactly
        // the one the session on its way out would swallow.
        self.begin_swap(&request.slot);
        let cwd = PathBuf::from(&request.cwd);
        let transcript_path = transcript_path(self.config_dir(), &cwd, &request.from);
        let board: Vec<Task> = self
            .board_rows(request.slot.project(), SystemTime::now(), crate::board::DEFAULT_STALE_SECS)
            .into_iter()
            .map(|row| row.task)
            .collect();
        let seat = HandoffSeat {
            slot: &request.slot,
            from: &request.from,
            cwd: &cwd,
            transcript_path: &transcript_path,
            account: request.account.clone(),
            model: request.model.clone(),
            permission_mode: request.permission_mode.clone(),
            role: request.role.clone(),
        };
        let truth = ground_truth(self.config_dir(), &seat, &board).await;
        let reads = truth.reads.clone();
        let record = HandoffRecord {
            slot: request.slot.clone(),
            from: request.from.clone(),
            to: request.to.clone(),
            reason: request.reason,
            fill_percent: request.fill.map(|fill| fill.percent),
            total_tokens: request.fill.map(|fill| fill.total_tokens),
            max_tokens: request.fill.map(|fill| fill.max_tokens),
            compaction_count: request.compaction_count,
            at_percent: request.at_percent,
            brief: HandoffBrief { ground_truth: truth.sections, judgment: request.judgment },
            transcript_path: transcript_path.display().to_string(),
            at: crate::install::rfc3339_now(),
            cycle: truth.cycle,
            successor: None,
        };
        let text = brief_text(&record.pair(), &record.brief);
        self.record_handoff(&record, &reads);
        // The brief is held before the row and the spawn, so a successor
        // that connects immediately cannot outrun it.
        self.set_pending_brief(&request.slot, PendingBrief { to: request.to.clone(), text });
        let _ = self
            .update_tx
            .send(SessionUpdate::Handoff { key: request.slot.clone(), handoff: record });
        if let Err(error) = self.dispatch(Command::NewSession {
            key: request.slot.clone(),
            cwd: request.cwd,
            launch_settings: request.launch_settings,
            session_id: Some(request.to),
        }) {
            self.clear_pending_brief(&request.slot);
            let held = self.end_swap(&request.slot);
            if !held.is_empty() {
                tracing::warn!(
                    target: "forge_workspace::handoff",
                    event_name = "handoff_prompts_dropped",
                    slot = %request.slot.display(),
                    held = held.len(),
                    "the handoff could not start its successor; prompts held for it were dropped",
                );
            }
            let _ = self.update_tx.send(SessionUpdate::Notice {
                key: request.slot.clone(),
                severity: NoticeSeverity::Error,
                text: format!("the handoff could not start a new session: {error}"),
            });
        }
    }
}

/// Where a session's transcript lives: the project directory the run's cwd
/// hashes to, then the id.
pub(crate) fn transcript_path(config_dir: &Path, cwd: &Path, session_id: &str) -> PathBuf {
    let cwd = cwd.to_string_lossy();
    let key = forge_agent::userdata::catalog::scan::project_key_for_directory(
        (!cwd.is_empty()).then_some(cwd.as_ref()),
    );
    forge_sdk::projects_dir_for(config_dir).join(key).join(format!("{session_id}.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_primitives::messages::{AssistantEnvelope, Extras, Usage, UserEnvelope};
    use forge_primitives::tasks::{LinkKind, TaskLink};

    fn user(text: &str) -> Message {
        Message::User {
            message: UserEnvelope {
                role: "user".to_owned(),
                content: vec![ContentBlock::Text { text: text.to_owned(), extras: Extras::new() }],
                extras: Extras::new(),
            },
            session_id: "s".to_owned(),
            parent_tool_use_id: None,
            uuid: None,
            tool_use_result: None,
            timestamp: None,
            synthetic: false,
            extras: Extras::new(),
        }
    }

    fn assistant(content: Vec<ContentBlock>, usage: Option<Usage>) -> Message {
        Message::Assistant {
            message: AssistantEnvelope {
                id: "m".to_owned(),
                role: "assistant".to_owned(),
                model: "a-model".to_owned(),
                content,
                stop_reason: None,
                stop_sequence: None,
                usage,
                extras: Extras::new(),
            },
            session_id: "s".to_owned(),
            parent_tool_use_id: None,
            error: None,
            uuid: None,
            timestamp: None,
            extras: Extras::new(),
        }
    }

    fn text(value: &str) -> ContentBlock {
        ContentBlock::Text { text: value.to_owned(), extras: Extras::new() }
    }

    fn call(id: &str, name: &str, input: serde_json::Value) -> Message {
        assistant(
            vec![ContentBlock::ToolUse {
                id: id.to_owned(),
                name: name.to_owned(),
                input,
                extras: Extras::new(),
            }],
            None,
        )
    }

    fn result(id: &str, output: &str, is_error: bool) -> Message {
        Message::User {
            message: UserEnvelope {
                role: "user".to_owned(),
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: id.to_owned(),
                    content: serde_json::Value::String(output.to_owned()),
                    is_error,
                    extras: Extras::new(),
                }],
                extras: Extras::new(),
            },
            session_id: "s".to_owned(),
            parent_tool_use_id: None,
            uuid: None,
            tool_use_result: None,
            timestamp: None,
            synthetic: false,
            extras: Extras::new(),
        }
    }

    fn usage(input: u64, cache_read: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: 10,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: cache_read,
            extras: Extras::new(),
        }
    }

    fn task(subject: &str, owner: Option<SessionSlot>, link: Option<&str>) -> Task {
        Task {
            id: forge_primitives::TaskId::from(subject),
            project_name: "forge".to_owned(),
            subject: subject.to_owned(),
            active_form: None,
            detail: None,
            status: TaskStatus::InProgress,
            owner,
            parent: None,
            waiting_on: None,
            estimate: None,
            rank: None,
            verify: None,
            attested: None,
            reserved: None,
            links: link
                .map(|target| TaskLink {
                    kind: LinkKind::Pr,
                    label: None,
                    target: target.to_owned(),
                    state: None,
                    added_at: SystemTime::UNIX_EPOCH,
                })
                .into_iter()
                .collect(),
            attempt: 0,
            archived_at: None,
            created_at: SystemTime::UNIX_EPOCH,
            updated_at: SystemTime::UNIX_EPOCH,
        }
    }

    fn seat() -> HandoffSeat<'static> {
        HandoffSeat {
            slot: Box::leak(Box::new(SessionSlot::worker("Busytools", "forge", "handoff-cut"))),
            from: "9cfe315c-aaaa-bbbb-cccc-dddddddddddd",
            cwd: Path::new("/tmp/forge"),
            transcript_path: Path::new("/tmp/projects/forge/9cfe315c.jsonl"),
            account: Some("Personal".to_owned()),
            model: Some("claude-opus-5-5".to_owned()),
            permission_mode: Some("auto".to_owned()),
            role: Role::Worker { charter_opening: Some("You build the cut.".to_owned()) },
        }
    }

    /// The seat and the record are what tell the successor which session
    /// this brief belongs to, and where the rest of the truth is.
    #[test]
    fn the_seat_and_the_record_name_the_identity_and_the_transcript() {
        let ground = assemble(&seat(), &[], &HandoffGitState::default(), &[]);
        let seat_lines = &ground.sections[0].lines;
        assert_eq!(ground.sections[0].label, "seat");
        assert!(
            seat_lines.contains(&"Busytools/forge/handoff-cut".to_owned()),
            "the slot: {seat_lines:?}",
        );
        assert!(
            seat_lines.contains(&"not a git repository".to_owned()),
            "a directory git says nothing about is stated: {seat_lines:?}",
        );
        assert!(
            seat_lines.contains(&"account Personal".to_owned())
                && seat_lines.contains(&"model claude-opus-5-5".to_owned())
                && seat_lines.contains(&"mode auto".to_owned()),
            "and what the seat runs as: {seat_lines:?}",
        );
        let record = ground.sections.last().expect("the record");
        assert_eq!(record.label, "record");
        assert!(
            record.lines.contains(&"/tmp/projects/forge/9cfe315c.jsonl".to_owned()),
            "the transcript path: {:?}",
            record.lines,
        );
        assert!(
            record.lines.contains(&"session 9cfe315c-aaaa-bbbb-cccc-dddddddddddd".to_owned()),
            "and the session id: {:?}",
            record.lines,
        );
        let role = ground.sections.iter().find(|section| section.label == "role").expect("role");
        assert_eq!(
            role.lines,
            vec!["worker, charter opens: You build the cut.".to_owned()],
            "the role line says which charter this brief belongs to",
        );
    }

    /// The brief opens on the session's intent and carries the last three
    /// prompts, so a successor sees the ask that started it and the words
    /// that moved it since.
    #[test]
    fn the_ask_names_the_first_prompt_and_the_newest_three() {
        let messages: Vec<Message> =
            ["one", "two", "three", "four", "five"].iter().map(|text| user(text)).collect();
        let asks = user_prompts(&messages);
        let section = ask_section(&asks);
        assert_eq!(
            section.lines,
            vec![
                "first: one".to_owned(),
                "#3: three".to_owned(),
                "#4: four".to_owned(),
                "#5: five".to_owned(),
            ],
            "the first ask plus the newest three, oldest first",
        );
    }

    /// A constraint is quoted exactly as it was written, with the prompt it
    /// came from: paraphrase is where "Edit/Write only" becomes "prefer the
    /// edit tools".
    #[test]
    fn a_constraint_is_quoted_verbatim_with_the_prompt_it_came_from() {
        let prompts = user_prompts(&[
            user("Please fix the login bug.\nHard rule: Edit/Write only, never sed -i."),
            user("and open a PR"),
        ]);
        let section = constraint_section(&prompts);
        assert_eq!(
            section.lines,
            vec!["\"Hard rule: Edit/Write only, never sed -i.\" (user prompt 1)".to_owned(),],
            "the rule verbatim, with its source",
        );
    }

    /// The brief names what changed, what was read and what is still
    /// uncommitted: the successor cannot infer any of it from the ask.
    #[test]
    fn the_files_section_names_edits_reads_and_the_uncommitted_paths() {
        let messages = vec![
            call("t1", "Read", serde_json::json!({ "file_path": "src/lib.rs" })),
            call("t2", "Edit", serde_json::json!({ "file_path": "src/handoff.rs" })),
            call("t3", "Write", serde_json::json!({ "file_path": "docs/handoff.md" })),
        ];
        let git = HandoffGitState {
            branch: Some("worktree-handoff-cut".to_owned()),
            head: Some("94cab2f20".to_owned()),
            dirty: vec![" M src/handoff.rs".to_owned()],
            diff: vec![" src/handoff.rs | 10 +++++-----".to_owned()],
        };
        let section = files_section(&messages, &git);
        assert!(
            section.lines.contains(&"changed: src/handoff.rs, docs/handoff.md".to_owned()),
            "both edits are named: {:?}",
            section.lines,
        );
        assert!(
            section.lines.contains(&"read: src/lib.rs".to_owned()),
            "the read is named: {:?}",
            section.lines,
        );
        assert!(
            section.lines.contains(&" M src/handoff.rs".to_owned()),
            "the uncommitted path rides whatever git said: {:?}",
            section.lines,
        );
        assert!(
            section.lines.iter().any(|line| line.starts_with("uncommitted: 1 paths")),
            "and the count is stated: {:?}",
            section.lines,
        );
    }

    /// A failed gate is reported as failed, with the line that names the
    /// failure: a brief that says the gate is green when it is not is worse
    /// than no brief.
    #[test]
    fn the_verify_section_carries_the_gate_verdict_and_the_failing_test() {
        let messages = vec![
            call("g1", "Bash", serde_json::json!({ "command": "just check" })),
            result(
                "g1",
                "   FAIL [   0.020s] forge-workspace config::tests::a_handoff_percent",
                true,
            ),
            assistant(vec![text("the gate read 3 failures across 1,900 tests, 41 s wall")], None),
        ];
        let section = verify_section(&messages);
        assert!(
            section.lines.contains(&"gate: just check -> failed".to_owned()),
            "the gate and its verdict: {:?}",
            section.lines,
        );
        assert!(
            section.lines.iter().any(|line| line.contains("FAIL [   0.020s]")),
            "the failing test is named: {:?}",
            section.lines,
        );
        assert!(
            section.lines.iter().any(|line| line.starts_with("measured: the gate read 3 failures")),
            "and the last measured line is carried: {:?}",
            section.lines,
        );
    }

    /// Only the slot's own rows are its work, and their links are what say
    /// which PR and issue the successor is continuing.
    #[test]
    fn the_board_section_lists_only_the_slots_own_rows_and_their_links() {
        let mine = SessionSlot::worker("Busytools", "forge", "handoff-cut");
        let rows = vec![
            task("the automatic handoff", Some(mine.clone()), Some("https://github.com/x/pull/9")),
            task(
                "someone else's work",
                Some(SessionSlot::worker("Busytools", "forge", "other")),
                None,
            ),
        ];
        let section = board_section(&mine, &rows);
        assert_eq!(
            section.lines.first().map(String::as_str),
            Some("the automatic handoff (in progress)"),
            "the owned row is drawn with its state",
        );
        assert!(
            section.lines.contains(&"pr: https://github.com/x/pull/9".to_owned()),
            "and its link: {:?}",
            section.lines,
        );
        assert!(
            !section.lines.iter().any(|line| line.contains("someone else")),
            "another seat's row is not the successor's work: {:?}",
            section.lines,
        );
    }

    /// The cycle counts one request per assistant row that has a usage
    /// block, one turn per real user prompt, and every prompt token the
    /// model read - cache reads included.
    #[test]
    fn the_cycle_counts_requests_turns_and_the_prompt_tokens_the_model_read() {
        let messages = vec![
            user("go"),
            assistant(vec![text("working")], Some(usage(100, 1_000))),
            result("g1", "ok", false),
            user("and now this"),
            assistant(vec![text("done")], Some(usage(200, 2_000))),
        ];
        let cycle = cycle(&messages, user_prompts(&messages).len());
        assert_eq!(cycle.requests, 2, "one request per usage-bearing assistant row");
        assert_eq!(cycle.turns, 2, "one turn per real user prompt");
        assert_eq!(cycle.prompt_tokens, 3_300, "input and cache reads both count");
    }

    /// A reply whose headings differ is still the successor's brief: the
    /// row draws what the old session wrote, not nothing.
    #[test]
    fn a_heading_less_judgment_is_one_section_rather_than_nothing() {
        let sections = parse_judgment("the work is half done.\nthe next step is the client row.");
        assert_eq!(sections.len(), 1, "one section, not zero");
        assert_eq!(sections[0].label, "judgment");
        assert_eq!(sections[0].lines.len(), 2, "and its lines survive");
    }

    /// The five headings forge asks for parse into five sections, in order.
    #[test]
    fn the_five_headings_parse_in_order() {
        let raw = "Now: the cut is half in.\nTried and failed: the first spawn.\nDo not: delete the row.\nNext: the client row.\nDone means: the gates are green.";
        let sections = parse_judgment(raw);
        let labels: Vec<&str> = sections.iter().map(|section| section.label.as_str()).collect();
        assert_eq!(labels, JUDGMENT_HEADINGS.to_vec(), "the five, in the order asked for");
        assert_eq!(sections[1].lines, vec!["the first spawn.".to_owned()]);
    }

    /// The successor reads the facts before the judgment, under the two
    /// part headings.
    #[test]
    fn the_brief_puts_the_ground_truth_before_the_judgment() {
        let brief = HandoffBrief {
            ground_truth: vec![HandoffSection {
                label: "seat".to_owned(),
                lines: vec!["Busytools/forge/lead".to_owned()],
            }],
            judgment: vec![HandoffSection {
                label: "Next".to_owned(),
                lines: vec!["the client row".to_owned()],
            }],
        };
        let text = brief_text("9cfe315c -> 4b21e0a7", &brief);
        let ground = text.find("PART 1: GROUND TRUTH").expect("part 1");
        let judgment = text.find("PART 2: JUDGMENT").expect("part 2");
        assert!(ground < judgment, "ground truth first: {text}");
        assert!(text.contains("seat: Busytools/forge/lead"), "the facts are rendered");
        assert!(text.contains("Next: the client row"), "and so is the judgment");
    }

    /// Every machinery delivery goes through one gate, so a cron, a peer
    /// message, a Gotify push and a Slack message all wait for the successor
    /// rather than starting a turn on the session that is leaving.
    #[test]
    fn a_machinery_prompt_waits_for_the_successor() {
        use crate::protocol::PromptSource;

        let (workspace, _rx) = crate::Workspace::testing_stub();
        let slot = SessionSlot::lead("Busytools", "forge");
        workspace.begin_swap(&slot);

        workspace
            .dispatch_workspace_prompt_under(
                &slot,
                "the cron fired".to_owned(),
                PromptSource::Cron,
                "cron-1".to_owned(),
            )
            .expect("a held prompt is an accepted prompt");

        let held = workspace.end_swap(&slot);
        assert_eq!(held.len(), 1, "the delivery is held, not run: {held:?}");
        assert_eq!(held[0].text, "the cron fired");
        assert_eq!(held[0].source, PromptSource::Cron, "and it keeps who sent it");
        assert!(!workspace.swap_in_progress(&slot), "the gate closes with the take");
    }

    /// The successor's own first user row is the brief forge gave it, not a
    /// turn of its own - and its judgment half can carry a "do not" rule
    /// that would otherwise read as a correction.
    #[test]
    fn the_successor_counts_skip_the_brief_row() {
        let messages = vec![
            user("HANDOFF - 9cfe315c -> 4b21e0a7\n\nPART 2: JUDGMENT\ndo not delete the row"),
            user("carry on"),
            user("actually, keep the row"),
        ];
        let counts = successor_counts(&messages, &[]);
        assert_eq!(counts.turns, 2, "two turns, not the brief");
        assert_eq!(counts.corrections, 1, "and the correction is the later prompt: {counts:?}");
    }

    /// A re-read is a file the PREDECESSOR had read: the same file the
    /// successor opened for its own reasons is not evidence the brief lost
    /// anything.
    #[test]
    fn a_reread_is_a_file_the_predecessor_had_read() {
        let messages = vec![
            call("r1", "Read", serde_json::json!({ "file_path": "src/a.rs" })),
            call("r2", "Read", serde_json::json!({ "file_path": "src/b.rs" })),
        ];
        let counts = successor_counts(&messages, &["src/a.rs".to_owned()]);
        assert_eq!(counts.reread_files, 1, "only the file both sessions read: {counts:?}");
    }

    /// Questions and corrections are what say the successor could not pick
    /// the work up from the brief alone.
    #[test]
    fn questions_and_corrections_are_counted() {
        let messages = vec![
            user("HANDOFF - brief"),
            user("go on"),
            call("q1", "AskUserQuestion", serde_json::json!({ "questions": [] })),
            user("as i said, the server row first"),
        ];
        let counts = successor_counts(&messages, &[]);
        assert_eq!(counts.questions, 1, "the ask is counted");
        assert_eq!(counts.corrections, 1, "and the repeat is a correction: {counts:?}");
    }

    /// A line is "measured" only when it states a number in a unit: prose
    /// with a digit in it is not a measurement.
    #[test]
    fn a_measured_line_needs_a_number_in_a_unit() {
        let messages = vec![
            assistant(vec![text("we are on step 3 of 5")], None),
            assistant(vec![text("the gate finished in 1.2 s")], None),
        ];
        assert_eq!(
            measured_line(&messages).as_deref(),
            Some("the gate finished in 1.2 s"),
            "the number with its unit, not the bare digit",
        );
    }
}
