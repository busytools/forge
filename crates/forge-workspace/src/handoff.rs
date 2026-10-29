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
const ACTION_CAP: usize = 10;
const DIRTY_CAP: usize = 20;
const CONSTRAINT_CAP: usize = 10;
const CONSTRAINT_LINE_CAP: usize = 300;
const CONSTRAINT_BLOCK_CAP: usize = 6;
const PROMPT_LINE_CAP: usize = 600;
const MEASURED_LINE_CAP: usize = 200;
/// The judgment half's ceiling: room for the 400-word note the prompt asks
/// for, with its pointer lines, and no more.
const JUDGMENT_CAP: usize = 5_000;

/// The carried judgment turn's ceiling, in JSON bytes.
///
/// **A turn is not a transcript, and this one rides every seat read** - the
/// record crosses the socket on every snapshot and is written to the store
/// per handoff. One turn of a procedure that reads a large file must not
/// become a megabyte on a page read; generous enough that an ordinary turn
/// (measured at ~42k in the scratch stack) fits whole.
const JUDGMENT_TURN_CAP: usize = 256 * 1024;

/// The parts of the judgment half forge asks for, in order. The note is the
/// position and the index rather than a retelling: the successor opens what
/// `Pointers` names instead of reading a summary of it.
pub(crate) const JUDGMENT_HEADINGS: [&str; 5] =
    ["Now", "Where it stood", "Pointers", "Next", "Done means"];

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
    /// The turn the procedure ran in, for a view attaching after the swap.
    pub judgment_turn: Vec<Message>,
}

/// How many of the successor's turns are measured.
pub(crate) const SUCCESSOR_TURNS: u32 = 10;

/// How long a handoff waits for its judgment prompt to start.
///
/// A prompt that never starts never produces the turn whose Result would
/// finish the handoff, and a CLI that advertises no lifecycle frames never
/// says it started at all - so the bound is the only backstop left once the
/// cancellation and settlement frames have said nothing.
pub(crate) const JUDGMENT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(600);

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
    prior: Option<&HandoffRecord>,
    live: Vec<HandoffSection>,
) -> GroundTruth {
    let read = {
        let config_dir = config_dir.to_path_buf();
        let session_id = seat.from.to_owned();
        let cwd = seat.cwd.to_path_buf();
        // The read is the whole transcript; the brief needs its newest asks
        // and the calls between, which only a pass over the file can answer.
        tokio::task::spawn_blocking(move || {
            forge_agent::session_history(&config_dir, &session_id, &cwd.to_string_lossy())
        })
        .await
    };
    // **An absent transcript is a read that failed, not an empty session.**
    // The loader answers a missing file with an empty history, so the path
    // itself is the only thing that can tell the two apart.
    let read_failed = read.is_err() || !seat.transcript_path.exists();
    let messages = read.map(|history| history.messages).unwrap_or_default();
    let git = forge_agent::env::git_state::probe(seat.cwd).await;
    // **The handoff writes memory itself, so that directory is not the
    // session's work.** Counting it in the files row would displace the
    // project files the successor needs, and counting its reads would make
    // the successor's re-read measurement measure the handoff's own cost.
    let memory_roots = memory_roots(config_dir, seat.cwd);
    let mut truth = assemble(seat, &messages, &git, board, prior, &memory_roots, live);
    if read_failed {
        // **An empty PART 1 is not a session with nothing in it.** Without
        // this line the successor reads a blank ground truth as a blank
        // session, when what happened is that its predecessor's transcript
        // could not be read at all.
        truth.sections.insert(
            0,
            HandoffSection {
                label: "note".to_owned(),
                lines: vec![
                    "the previous session's transcript could not be read; the asks, files, \
                     verification and record below are empty for that reason"
                        .to_owned(),
                ],
            },
        );
    }
    truth
}

/// The deterministic half, pure over its inputs: what the tests drive.
pub(crate) fn assemble(
    seat: &HandoffSeat<'_>,
    messages: &[Message],
    git: &HandoffGitState,
    board: &[Task],
    prior: Option<&HandoffRecord>,
    memory_roots: &[PathBuf],
    live: Vec<HandoffSection>,
) -> GroundTruth {
    let asks = user_prompts(messages);
    let mut ask = ask_section(&asks);
    let mut constraints = constraint_section(&asks);
    if let Some(prior) = prior {
        // **A successor's own transcript begins with the brief, which the
        // machine filter removes.** Without carrying those lines forward, a
        // second handoff would have no first ask and no constraints - the
        // original intent is in the record the session arrived by, not in
        // the transcript above it.
        let carried_asks = section_lines(&prior.brief.ground_truth, "ask");
        if !carried_asks.is_empty() {
            let mut lines = vec!["carried by the brief this session arrived by".to_owned()];
            lines.extend(carried_asks);
            if !ask.lines.is_empty() {
                lines.push("since then".to_owned());
                lines.extend(std::mem::take(&mut ask.lines));
            }
            ask.lines = lines;
        }
        let carried_rules = section_lines(&prior.brief.ground_truth, "constraints");
        if !carried_rules.is_empty() {
            for rule in &carried_rules {
                if !constraints.lines.contains(rule) {
                    constraints.lines.insert(0, rule.clone());
                }
            }
            if constraints.lines.len() > CONSTRAINT_CAP {
                let cut = constraints.lines.len() - CONSTRAINT_CAP;
                constraints.lines.truncate(CONSTRAINT_CAP);
                constraints.lines.push(format!("[cut; {cut} more lines]"));
            }
        }
    }
    let mut sections = seat_sections(seat, git);
    sections.push(role_section(&seat.role));
    sections.push(ask);
    sections.push(constraints);
    sections.push(files_section(messages, git, memory_roots));
    // **No action, no heading.** A brief with a blank "actions" label reads as
    // a section the reader should have filled in rather than one that does not
    // apply to this session.
    sections.extend(actions_section(messages));
    sections.push(verify_section(messages));
    sections.push(board_section(seat.slot, board));
    sections.extend(live);
    sections.push(record_section(seat));
    GroundTruth {
        sections,
        cycle: cycle(messages, asks.len()),
        reads: read_set(messages, memory_roots),
        judgment_turn: judgment_turn(messages),
    }
}

/// The turn the handing-over session ran the handoff procedure in.
///
/// **From the procedure prompt to the end of the transcript.** The prompt is
/// forge's own row and everything after it is the turn. The LAST match is
/// taken because a handoff that was abandoned still leaves its prompt in the
/// transcript, and the turn a view draws is the one that actually handed
/// over.
fn judgment_turn(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .rposition(is_procedure_prompt)
        .map_or_else(Vec::new, |at| cap_turn(&messages[at..]))
}

/// Keep the turn under [`JUDGMENT_TURN_CAP`]: the prompt always, then the
/// newest of the rest that fits, and one line saying what was left out.
///
/// **The cut is stated, never silent.** The reader sees a turn that ends with
/// an omission row pointing at the transcript, rather than a short turn that
/// reads as the whole of what the old session did.
///
/// **The weight is the serialized ARRAY**, separators included, because that
/// is what the record carries and what the wire measures. A message that will
/// not serialize counts as the whole ceiling rather than as nothing, so a
/// failure can only shorten the turn, never overflow it. A prompt that is
/// itself past the ceiling is carried alone: dropping the row the turn is
/// recognised by would cost more than the bound is worth.
fn cap_turn(turn: &[Message]) -> Vec<Message> {
    let weigh = |messages: &[Message]| {
        serde_json::to_string(messages).map_or(usize::MAX, |json| json.len())
    };
    if weigh(turn) <= JUDGMENT_TURN_CAP {
        return turn.to_vec();
    }
    let Some(prompt) = turn.first() else { return Vec::new() };
    let rest = &turn[1..];
    // Keep the newest messages that fit, dropping from the oldest end. The
    // common case is a step or two; a turn with many large messages pays one
    // serialization each, once per handoff.
    let mut take = rest.len();
    loop {
        let mut capped = Vec::with_capacity(take + 2);
        capped.push(prompt.clone());
        capped.extend(rest[rest.len() - take..].iter().cloned());
        capped.push(omission(rest.len() - take));
        if weigh(&capped) <= JUDGMENT_TURN_CAP || take == 0 {
            return capped;
        }
        take -= 1;
    }
}

/// The row a capped turn ends on: a notice, so the fold draws it as the
/// machinery's line rather than the reader's words.
///
/// **A stable id, because the record is seeded more than once.** A view folds
/// the turn from every seat read and reconnect; a frame with no id is one
/// nothing can pair, so each seed would append the line again and the cut
/// would multiply down the turn. `uuid` flattens onto the frame, which is
/// what the client's own matching reads.
fn omission(dropped: usize) -> Message {
    Message::System {
        subtype: "forge_notice".to_owned(),
        session_id: None,
        data: serde_json::json!({
            "uuid": HANDOFF_TURN_CUT_ID,
            "severity": "info",
            "text": format!(
                "[cut; {dropped} earlier messages of this turn are in the previous session - \
                 resume it from the handoff row]"
            ),
        }),
    }
}

/// The omission row's id, shared so a reader that pairs frames by id knows
/// the line is one fact however often the record is seeded.
pub(crate) const HANDOFF_TURN_CUT_ID: &str = "forge_handoff_turn_cut";

/// Whether a row is the handoff procedure's own prompt: minted under the
/// procedure's prefix and opening on the words the machinery matcher reads.
///
/// **Both halves, not the words alone.** A reader can type the opening as
/// easily as the machinery writes it, and the id is the only fact about the
/// sender the transcript keeps.
///
/// **Two carriers, because the CLI has two.** The prompt normally lands as
/// the row's own words; one queued behind a running turn lands as a
/// `queued_command` block carrying the prompt's id on `source_uuid` instead,
/// which is the same pairing the client's fold reads. Missing that carrier
/// would draw the row live and carry no turn at all on a reload.
fn is_procedure_prompt(message: &Message) -> bool {
    let Message::User { uuid, message: envelope, .. } = message else { return false };
    let minted = |id: &str| id.starts_with(forge_sdk::request_id::HANDOFF_PROMPT_PREFIX);
    let opens = |text: &str| text.trim_start().starts_with(MACHINE_PREFIX);
    if uuid.as_deref().is_some_and(minted)
        && text_of(&envelope.content).is_some_and(|text| opens(&text))
    {
        return true;
    }
    envelope.content.iter().any(|block| match block {
        ContentBlock::QueuedCommand { prompt, source_uuid, .. } => {
            source_uuid.as_deref().is_some_and(minted) && opens(&result_text(prompt))
        }
        _ => false,
    })
}

/// A section's lines from a brief, by label. An older record's brief is the
/// only place its asks survive: the transcript above the successor is gone.
fn section_lines(sections: &[HandoffSection], label: &str) -> Vec<String> {
    sections
        .iter()
        .find(|section| section.label == label)
        .map(|section| section.lines.clone())
        .unwrap_or_default()
}

/// The directories the handoff's own memory writes land in: the project's
/// memory dir, and the environment's own where the cross-project notes live.
///
/// **Both, not only the project's.** Either one in the files row would
/// displace the project files the successor needs, and either read would
/// measure the handoff's own cost as the predecessor's.
fn memory_roots(config_dir: &Path, cwd: &Path) -> Vec<PathBuf> {
    let project = forge_agent::userdata::memory::project_memory_path(config_dir, cwd);
    [project.parent().map(Path::to_path_buf), Some(config_dir.join("memory"))]
        .into_iter()
        .flatten()
        .collect()
}

/// Whether `path` is inside one of the memory directories.
///
/// **The handoff turn writes memory itself.** Counting those paths would
/// displace the project files the successor needs, and counting their reads
/// would measure the successor against the handoff's own cost.
fn is_memory_path(path: &str, memory_roots: &[PathBuf]) -> bool {
    // **The same file can be spelled more than one way.** A `~/...` path and a
    // symlinked root both name the directory the memory lives in, so every
    // spelling on either side is compared.
    let forms = |path: &Path| {
        let spelled = path.to_string_lossy().into_owned();
        let expanded = match spelled.strip_prefix("~/") {
            Some(rest) => dirs::home_dir().map_or_else(
                || spelled.clone(),
                |home| home.join(rest).to_string_lossy().into_owned(),
            ),
            None => spelled.clone(),
        };
        let canonical = std::fs::canonicalize(path)
            .or_else(|_| std::fs::canonicalize(&expanded))
            .map(|path| path.to_string_lossy().into_owned())
            .ok();
        let mut forms = vec![spelled, expanded];
        if let Some(canonical) = canonical {
            forms.push(canonical);
        }
        forms
    };
    let given = forms(Path::new(path));
    memory_roots.iter().any(|root| {
        let roots = forms(root);
        roots.iter().any(|root| {
            given.iter().any(|path| {
                path == root
                    || path.strip_prefix(root.as_str()).is_some_and(|rest| rest.starts_with('/'))
            })
        })
    })
}

/// Every path the session read, in transcript order, deduped and capped.
fn read_set(messages: &[Message], memory_roots: &[PathBuf]) -> Vec<String> {
    let mut reads: Vec<String> = Vec::new();
    for (_, name, input) in tool_uses(messages) {
        if !matches!(name, "Read" | "NotebookRead") {
            continue;
        }
        let Some(path) = input.get("file_path").or_else(|| input.get("notebook_path")) else {
            continue;
        };
        let Some(path) = path.as_str() else { continue };
        if is_memory_path(path, memory_roots) {
            continue;
        }
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
                // **The machine's own rows are not turns.** The brief, a task
                // notice, an interrupt marker and the no-output nudge all
                // arrive as ordinary user rows, and counting one would
                // measure the successor against the harness's words.
                if is_machine_prompt(message) {
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

/// The counts, or `None` when the transcript read answered nothing.
///
/// **A failed read is not a successor that re-read nothing.** The read is a
/// whole transcript; a successor with turns to count cannot have an empty
/// one, so an empty answer is a failure and writing zeros for it would read
/// as the best possible brief.
fn successor_measurement(
    messages: &[Message],
    predecessor_reads: &[String],
) -> Option<forge_primitives::HandoffSuccessor> {
    if messages.is_empty() {
        return None;
    }
    let counts = successor_counts(messages, predecessor_reads);
    // A transcript holding only the brief and the assistant's rows has no
    // turn to measure: zeros would read as a successor that re-read nothing,
    // which is the best possible brief.
    (counts.turns > 0).then_some(counts)
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

/// The seat's facts, one section per label: the brief draws a table, and a
/// fact carried inside another section's value has no row of its own.
fn seat_sections(seat: &HandoffSeat<'_>, git: &HandoffGitState) -> Vec<HandoffSection> {
    let mut sections =
        vec![fact("seat", seat.slot.display()), fact("cwd", seat.cwd.display().to_string())];
    sections.push(match (git.branch.as_deref(), git.head.as_deref()) {
        (Some(branch), Some(head)) => fact("branch", format!("{branch} @ {head}")),
        (Some(branch), None) => fact("branch", branch.to_owned()),
        (None, _) => fact("branch", "not a git repository".to_owned()),
    });
    if let Some(account) = &seat.account {
        sections.push(fact("account", account.clone()));
    }
    if let Some(model) = &seat.model {
        sections.push(fact("model", model.clone()));
    }
    if let Some(mode) = &seat.permission_mode {
        sections.push(fact("mode", mode.clone()));
    }
    sections
}

fn fact(label: &str, line: String) -> HandoffSection {
    HandoffSection { label: label.to_owned(), lines: vec![line] }
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
    for (ordinal, text) in &asks[recent..] {
        let (author, body) = prompt_author(text, *ordinal);
        lines.push(format!("#{ordinal} ({author}): {}", clipped_verbatim(body, PROMPT_LINE_CAP)));
    }
    HandoffSection { label: "ask".to_owned(), lines }
}

/// Who a user row came from, and the words without a peer envelope's own
/// header.
///
/// A peer message is an ordinary user row whose text opens with
/// `[Message id=... from agent 'X' (org 'Y')]`. Quoting it as "user prompt
/// N" would attribute another agent's words to the person.
fn prompt_author(text: &str, ordinal: usize) -> (String, &str) {
    if let Some(rest) = text.strip_prefix("[Message id=")
        && let Some((_id, rest)) = rest.split_once(" from agent '")
        && let Some((agent, rest)) = rest.split_once("' (org '")
        && let Some((org, body)) = rest.split_once("')]")
    {
        let author = format!("peer message from {agent} (org {org})");
        return (author, body.trim_start());
    }
    (format!("user prompt {ordinal}"), text)
}

/// The rules still binding, quoted. A cue word is the pattern and a heading
/// the user wrote is the other, so a constraint is picked out of the prompt
/// that stated it rather than paraphrased from it.
///
/// **A heading takes its block with it.** `Hard rule:` alone is the label of
/// a rule and none of it; the bullets under it are the rule, so a cue line
/// ending in `:` carries the contiguous indented or bulleted lines below it.
fn constraint_section(asks: &[(usize, String)]) -> HandoffSection {
    let mut found: Vec<String> = Vec::new();
    for (ordinal, text) in asks.iter().rev() {
        let (author, body) = prompt_author(text, *ordinal);
        let raw: Vec<&str> = body.lines().collect();
        for (at, raw_line) in raw.iter().enumerate().rev() {
            let line = raw_line.trim();
            if line.is_empty() || !states_a_rule(line) {
                continue;
            }
            let mut block = vec![line.to_owned()];
            if line.ends_with(':') {
                let mut cut = 0;
                for next in raw.iter().skip(at + 1) {
                    let trimmed = next.trim();
                    if trimmed.is_empty() {
                        break;
                    }
                    // Raw indentation is what tells a continuation from the
                    // next paragraph; the trimmed line cannot.
                    let continues = trimmed.starts_with("- ")
                        || trimmed.starts_with("* ")
                        || next.starts_with("  ")
                        || next.starts_with('\t');
                    if !continues {
                        break;
                    }
                    if block.len() >= CONSTRAINT_BLOCK_CAP {
                        cut += 1;
                        continue;
                    }
                    block.push(trimmed.to_owned());
                }
                if cut > 0 {
                    block.push(format!("[cut; {cut} more lines]"));
                }
            }
            let quoted = format!(
                "\"{}\" ({author})",
                clipped_verbatim(&block.join("\n"), CONSTRAINT_LINE_CAP)
            );
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

fn files_section(
    messages: &[Message],
    git: &HandoffGitState,
    memory_roots: &[PathBuf],
) -> HandoffSection {
    let mut changed: Vec<String> = Vec::new();
    let mut read_paths: Vec<String> = Vec::new();
    for (_, name, input) in tool_uses(messages) {
        let Some(path) = input.get("file_path").or_else(|| input.get("notebook_path")) else {
            continue;
        };
        let Some(path) = path.as_str() else { continue };
        if is_memory_path(path, memory_roots) {
            continue;
        }
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
        lines.push(list_line("changed", &changed, FILE_CAP));
    }
    if !read_paths.is_empty() {
        lines.push(list_line("read", &read_paths, FILE_CAP));
    }
    if git.dirty.is_empty() {
        lines.push("uncommitted: none".to_owned());
    } else {
        let shown = tail(&git.dirty, DIRTY_CAP);
        // The count is the tree's, not the list's: the probe caps what it
        // keeps, and a brief that said 20 for 60 paths loses the one thing
        // the line is for.
        let total = if git.dirty_total > 0 {
            usize::try_from(git.dirty_total).unwrap_or(usize::MAX)
        } else {
            git.dirty.len()
        };
        let omitted = total.saturating_sub(shown.len());
        if omitted > 0 {
            lines.push(format!(
                "uncommitted: {total} paths, newest {} shown ({omitted} omitted)",
                shown.len()
            ));
        } else {
            lines.push(format!("uncommitted: {total} paths"));
        }
        lines.extend(shown);
    }
    if !git.diff.is_empty() {
        lines.push("diff:".to_owned());
        lines.extend(git.diff.iter().cloned());
    }
    HandoffSection { label: "files".to_owned(), lines }
}

/// What the session DID through forge's own tools, one line each.
///
/// **A successor cannot infer these from the transcript above it**, because a
/// successor has none: an agent messaged, a row filed, a schedule set and a
/// page driven are all work that would otherwise vanish with the handover.
/// File and command families stay in their own sections above; the action's
/// own most telling argument rides the line so the verb alone is not a riddle.
fn actions_section(messages: &[Message]) -> Option<HandoffSection> {
    let mut lines: Vec<String> = Vec::new();
    for (_, name, input) in tool_uses(messages) {
        let Some(action) = forge_action(name) else { continue };
        let line = match action_detail(input) {
            Some(detail) => format!("{action}: {}", clipped(detail, MEASURED_LINE_CAP)),
            None => action.to_owned(),
        };
        if !lines.contains(&line) {
            lines.push(line);
        }
    }
    if lines.is_empty() {
        return None;
    }
    let (shown, omitted) = first_and_newest(&lines, ACTION_CAP);
    let mut out: Vec<String> = shown.into_iter().cloned().collect();
    if omitted > 0 {
        out.push(format!("... {omitted} more actions omitted"));
    }
    Some(HandoffSection { label: "actions".to_owned(), lines: out })
}

/// The forge tool families the brief carries as actions.
fn forge_action(name: &str) -> Option<&str> {
    let action = name.strip_prefix("mcp__forge__")?;
    let family = action.split("__").next().unwrap_or(action);
    let named = matches!(
        family,
        "agents" | "tasks" | "cron" | "slack" | "gotify" | "documents" | "review" | "systemone"
    );
    (named || family.starts_with("browser")).then_some(action)
}

/// The one argument of an action that says what it acted on.
fn action_detail(input: &serde_json::Value) -> Option<&str> {
    const KEYS: [&str; 8] =
        ["subject", "message", "prompt", "question", "text", "content", "description", "url"];
    KEYS.iter()
        .find_map(|key| input.get(*key).and_then(serde_json::Value::as_str))
        .filter(|value| !value.trim().is_empty())
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
    // The commands the session ran: the first plus the newest nine when the
    // list is long, with the omission stated rather than read as the whole.
    let (shown, omitted) = first_and_newest(&recent, COMMAND_CAP);
    for command in &shown {
        lines.push(format!("ran: {}", clipped(command, MEASURED_LINE_CAP)));
    }
    if omitted > 0 {
        lines.push(format!("ran: ... {omitted} more commands omitted"));
    }
    let gate = calls
        .into_iter()
        .rev()
        .find(|(_, command)| is_a_gate(command))
        .map(|(id, command)| (id, command.to_owned()));
    if let Some((id, command)) = gate {
        // **No recorded result is not a pass.** A gate whose output is not in
        // the transcript (a call still running, a result the window dropped)
        // is unknown, and a brief that called it ok would be the exact lie
        // this section exists to prevent.
        match results.get(&id) {
            Some((true, text)) => {
                lines.push(format!("gate: {} -> failed", clipped(&command, MEASURED_LINE_CAP)));
                let failing = failing_line(text).unwrap_or_else(|| "see the transcript".to_owned());
                lines.push(format!("failing: {failing}"));
            }
            Some((false, _)) => {
                lines.push(format!("gate: {} -> ok", clipped(&command, MEASURED_LINE_CAP)));
            }
            None => {
                lines.push(format!(
                    "gate: {} -> no result recorded",
                    clipped(&command, MEASURED_LINE_CAP)
                ));
            }
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

/// What is still running when the seat hands over: live sub-agents,
/// background tasks, monitors and the process tree.
///
/// **The successor has to know what to resume.** The note is an index and may
/// mention it, but the live set is a fact forge reads itself, so it rides in
/// the brief whether or not the note names it.
pub(crate) fn live_work_sections(
    domain: Option<&crate::domain_session::DomainSession>,
) -> Vec<HandoffSection> {
    // **An absent domain is unknown, not empty.** Every row says the same
    // thing rather than claiming nothing was running.
    let Some(domain) = domain else {
        return ["agents", "background", "monitors", "processes"]
            .into_iter()
            .map(|label| fact(label, "not read".to_owned()))
            .collect();
    };
    let agents: Vec<String> = domain
        .card_tracker
        .cards()
        .into_iter()
        .filter(|card| card.running)
        .map(|card| {
            let kind = card.agent_type.unwrap_or_else(|| "agent".to_owned());
            let backgrounded = if card.backgrounded { ", backgrounded" } else { "" };
            format!("{kind}: {} ({} calls{backgrounded})", card.name, card.calls)
        })
        .collect();
    let tasks: Vec<String> = domain
        .background_tasks
        .iter()
        .map(|task| format!("{}: {} [{}]", task.task_type, task.description, task.task_id))
        .collect();
    let watching: Vec<String> = domain
        .monitors
        .iter()
        .filter(|monitor| !monitor.status.is_terminal())
        .map(|monitor| {
            format!("{} [{}]: {}", monitor.description, monitor.tool_use_id, monitor.command)
        })
        .collect();
    let ps: Vec<String> = match &domain.process_snapshot {
        Some(snapshot) => snapshot
            .processes
            .iter()
            .take(FILE_CAP)
            .map(|process| format!("{} {}: {}", process.pid, process.name, process.command))
            .collect(),
        None => vec!["not scanned".to_owned()],
    };
    let section = |label: &str, lines: Vec<String>| HandoffSection {
        label: label.to_owned(),
        lines: if lines.is_empty() { vec!["none running".to_owned()] } else { lines },
    };
    vec![
        section("agents", agents),
        section("background", tasks),
        section("monitors", watching),
        section("processes", ps),
    ]
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
/// synthetic row is the harness speaking, a row with no text is a tool
/// result, and a machine prompt is forge's own words.
fn user_prompts(messages: &[Message]) -> Vec<(usize, String)> {
    let mut prompts = Vec::new();
    for message in messages {
        let Message::User { message: envelope, synthetic, .. } = message else { continue };
        if *synthetic || is_machine_prompt(message) {
            continue;
        }
        let Some(text) = text_of(&envelope.content) else { continue };
        prompts.push((prompts.len() + 1, text));
    }
    prompts
}

/// Whether a user row is forge's own prompt rather than a person's or a
/// peer's.
///
/// **A forged prompt is an ordinary user row in the transcript**, and the
/// forged id is the only mark the CLI keeps. Without this the judgment
/// prompt reads back as the newest ask (its own "Do not restate what is on
/// disk" line as a user constraint), and the previous brief becomes the next
/// handoff's first ask.
fn is_machine_prompt(message: &Message) -> bool {
    let Message::User { uuid, message: envelope, .. } = message else { return false };
    if uuid.as_deref().is_some_and(|id| id.starts_with("forge_nudge_")) {
        return true;
    }
    let Some(text) = text_of(&envelope.content) else { return false };
    let text = text.trim_start();
    text.starts_with("HANDOFF - ")
        || text.starts_with(MACHINE_PREFIX)
        || crate::conversation_turns::is_task_notice(text)
        || crate::conversation_turns::is_interrupt_marker(text)
        // The CLI's own nudge after an empty response; the client draws it as
        // its own line from the same opening (`noOutputNudge`).
        || text.starts_with("[Your previous response had no visible output.")
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

/// A list line: everything when it fits, and when it does not, the first
/// entry plus the newest `cap - 1` with the omission stated.
///
/// **The first is kept on purpose.** The file a successor first changed, or
/// the command that started the work, is usually the one it will need again;
/// a newest-only cut loses exactly that one.
fn list_line(label: &str, items: &[String], cap: usize) -> String {
    let (shown, omitted) = first_and_newest(items, cap);
    if omitted == 0 {
        return format!("{label}: {}", items.join(", "));
    }
    format!(
        "{label}: {} shown of {} ({omitted} omitted), first and newest: {}",
        shown.len(),
        items.len(),
        shown.iter().map(|item| item.as_str()).collect::<Vec<_>>().join(", "),
    )
}

/// The first entry plus the newest `cap - 1`, and how many that leaves out.
fn first_and_newest<T>(items: &[T], cap: usize) -> (Vec<&T>, usize) {
    if items.len() <= cap {
        return (items.iter().collect(), 0);
    }
    let newest = cap.saturating_sub(1);
    let mut shown: Vec<&T> = Vec::with_capacity(cap);
    shown.push(&items[0]);
    shown.extend(items[items.len() - newest..].iter());
    let omitted = items.len() - shown.len();
    (shown, omitted)
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

/// A quote clipped at `cap`, with the cut stated in words: the brief calls
/// these verbatim, so a shortened one must not read as the whole.
fn clipped_verbatim(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_owned();
    }
    let cut: String = text.chars().take(cap).collect();
    format!("{cut}... [cut; the full text is in the transcript]")
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

/// Whether a reply is a note: it parses into at least one heading section.
/// Anything else, including headings with no content under them, is prose or
/// an empty shell, and neither is the brief.
pub(crate) fn parses_as_note(raw: &str) -> bool {
    parse_judgment(raw).iter().any(|section| JUDGMENT_HEADINGS.contains(&section.label.as_str()))
}

/// The judgment half when no exit note arrived. **Never the turn's prose
/// dressed as the note**: the successor is told the note is missing and where
/// the session's own account lives.
pub(crate) fn missing_note() -> Vec<HandoffSection> {
    vec![HandoffSection {
        label: "note".to_owned(),
        lines: vec![
            "the handoff note did not arrive; this session's transcript is named in the record \
             above, and anything it left is there"
                .to_owned(),
        ],
    }]
}

/// The heading a line opens, and its body when the line carries one.
///
/// A model writes a heading as `Now: ...`, `## Now`, `**Now:** ...`,
/// `## Now.` or `1. **Now**` - the shipped skill numbers its own parts, and a
/// heading written to match it must still open. The label still has to open
/// the line, so prose that happens to begin with the word stays body text.
fn judgment_heading(line: &str) -> Option<(&'static str, &str)> {
    let text = heading_text(line);
    for heading in JUDGMENT_HEADINGS {
        let Some(rest) = text.strip_prefix(heading) else { continue };
        // `**Now:**` closes the emphasis around the label. Only that literal
        // pair comes off, so a body's own `*emphasis*` survives.
        let rest = strip_emphasis(rest);
        if let Some(body) = rest.strip_prefix(':') {
            return Some((heading, strip_emphasis(body).trim()));
        }
        if let Some(body) = rest.strip_prefix('.') {
            // A full stop ends the heading only when nothing follows it:
            // `Now. the work stands` is prose, not a heading with no body.
            if strip_emphasis(body).trim().is_empty() {
                return Some((heading, ""));
            }
            continue;
        }
        if rest.trim().is_empty() {
            return Some((heading, ""));
        }
    }
    None
}

/// The line with the markdown furniture a heading may wear taken off: the
/// `#`/`*` markers, and an ordered-list number (`1.`, `1)`).
fn heading_text(line: &str) -> &str {
    let text = line.trim_start_matches(['#', '*', '-', ' ']);
    match ordered_marker(text) {
        Some(rest) => rest.trim_start_matches(['#', '*', '-', ' ']),
        None => text,
    }
}

/// The text after a leading `1.` / `1)` marker, or `None` when there is none.
///
/// A list marker is followed by a space, which is what keeps `2024 report`
/// and `1.5 million` prose.
fn ordered_marker(text: &str) -> Option<&str> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let rest = text[digits..].strip_prefix('.').or_else(|| text[digits..].strip_prefix(')'))?;
    rest.starts_with(' ').then(|| rest.trim_start())
}

/// The label's own closing `**`, and only that literal pair.
fn strip_emphasis(text: &str) -> &str {
    text.strip_prefix("**").unwrap_or(text)
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

/// The prompt's opening. `is_machine_prompt` recognizes forge's own judgment
/// prompt by this prefix, so whatever procedure the turn runs, it opens here.
/// The prefix `is_machine_prompt` matches, shared so the prompt and the
/// matcher cannot drift apart.
const MACHINE_PREFIX: &str = "A handoff is starting for this session:";

/// The opening, built from the matcher's prefix so the two cannot drift.
fn judgment_opening() -> String {
    format!(
        "{MACHINE_PREFIX} forge will replace it with a new session that continues the work. \
         This turn has two jobs, in this order, and it ends on the text that becomes the \
         judgment half of the brief the new session is given."
    )
}

/// The handoff skill forge ships and installs into the config dir.
const SKILL: &str = include_str!("handoff/SKILL.md");

/// What the turn is told to do: run the skill, then exit through the tool.
const JUDGMENT_BODY: &str = "\
Invoke the handoff skill and follow it. It completes this session's learnings \
into memory, records and stops what is still running so the new session can \
resume it, then calls `agents__handoff` with the note. If this environment has \
no such skill, do those three steps from this prompt: memory first, then close \
out what is running, then call `agents__handoff` with the note.";

/// Forge's output contract for the note the exit call carries.
const JUDGMENT_CONTRACT: &str = "\
Write the note under exactly these five headings and no others, each alone at \
the start of its own line:\n\n\
Now: where the work stands, in two or three sentences - what is done and \
verified, what is in flight, and what is unchanged. Its last line names what you \
recorded in memory and where it lives.\n\
Where it stood: the exact position - the branch and commit, the row or PR in \
flight, the last command you ran and its verdict. A dead end is a fact here: \
name what was tried and what stopped it.\n\
Pointers: the index, one line each and no prose - the memory files by name, the \
rolling handoff document, the board rows by id, the PR numbers and the \
transcript path. The successor opens these instead of reading a summary of \
them.\n\
Next: the ordered next actions, each one bounded and starting at the file or \
command it begins with.\n\
Done means: the observable that ends this task.\n\n\
Do not restate what is on disk: forge's own half of the brief already carries \
the seat, the asks, the constraints, the files, the git state, the gate and the \
board. Never write a key, token, password or personal data into the note. This \
note is Step 2's handoff document, written under these five headings; pass the \
same text to `agents__handoff` as `brief`. Keep it under 400 words.";

/// The judgment prompt the turn runs: forge's own words, the skill it names,
/// and the output contract for the note the exit call carries.
pub(crate) fn judgment_prompt() -> String {
    format!("{}\n\n{JUDGMENT_BODY}\n\n{JUDGMENT_CONTRACT}", judgment_opening())
}

/// How an install left the skill on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillInstall {
    Created,
    Replaced,
    Unchanged,
}

/// Where the generated skill lives: `<config_dir>/skills/handoff/SKILL.md`.
pub fn skill_path(config_dir: &Path) -> PathBuf {
    config_dir.join("skills").join("handoff").join("SKILL.md")
}

/// Install the handoff skill at [`skill_path`].
///
/// **One path, one file.** forge.toml stays hand-authored and forge never
/// writes it; this generated skill is the one file forge owns inside the
/// config dir. It is compared byte for byte and rewritten, atomically through
/// a temp file in the same directory, only when the shipped text differs.
pub fn install_skill(config_dir: &Path) -> std::io::Result<SkillInstall> {
    let dir = config_dir.join("skills").join("handoff");
    let path = skill_path(config_dir);
    let outcome = match std::fs::read(&path) {
        Ok(existing) if existing == SKILL.as_bytes() => return Ok(SkillInstall::Unchanged),
        Ok(_) => SkillInstall::Replaced,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => SkillInstall::Created,
        Err(error) => return Err(error),
    };
    std::fs::create_dir_all(&dir)?;
    let temp = dir.join("SKILL.md.tmp");
    // **Nothing half-written is left behind.** The temp sits in the config
    // dir a sync daemon watches, so a failed write or rename removes it.
    if let Err(error) =
        std::fs::write(&temp, SKILL.as_bytes()).and_then(|()| std::fs::rename(&temp, &path))
    {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    Ok(outcome)
}

/// A prompt held because the seat was swapping its occupant.
#[derive(Debug, Clone)]
pub(crate) struct HeldPrompt {
    pub text: String,
    pub attachments: Vec<forge_primitives::ImageAttachment>,
    pub source: crate::protocol::PromptSource,
    /// The seat that sent it, when a peer message has one: a handoff that
    /// never reaches a successor answers that sender, the way the parked
    /// bucket does, rather than only telling the seat.
    pub sender: Option<SessionSlot>,
}

/// The handoff a session arrived by, while its first turns are measured.
#[derive(Debug, Clone)]
pub(crate) struct SuccessorWatch {
    /// The occupant the handoff named.
    pub to: String,
    /// Prompts counted so far, matching what `successor_counts` counts.
    pub turns: u32,
    /// What its predecessor had read.
    pub predecessor_reads: Vec<String>,
    /// The tenth prompt has run; the measurement is taken at its turn end,
    /// when the transcript holds all ten.
    pub measure_at_boundary: bool,
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
    /// The share, rounded, as the row draws it.
    pub percent: u8,
    pub total_tokens: u64,
    pub max_tokens: u64,
}

impl FillReading {
    /// Whether the seat has crossed `at_percent`.
    ///
    /// **Compared on the pair, not the rounded share.** Rounding first would
    /// fire a 84.6% reading at a 85% threshold; the record carries both
    /// numbers so a later reader can see the exact ratio the trigger saw.
    pub fn fires(&self, at_percent: u8) -> bool {
        if self.max_tokens == 0 {
            // A reading that named only its share: all this can compare.
            return self.percent >= at_percent;
        }
        self.total_tokens.saturating_mul(100)
            >= u64::from(at_percent).saturating_mul(self.max_tokens)
    }
}

/// One handoff, decided and ready to run.
pub(crate) struct SwapRequest {
    pub slot: SessionSlot,
    pub from: String,
    pub to: String,
    pub reason: HandoffReason,
    pub fill: Option<FillReading>,
    pub at_percent: u8,
    pub role: Role,
    pub account: Option<String>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    pub judgment: Vec<HandoffSection>,
    pub cwd: String,
    pub launch_settings: SessionLaunchSettings,
    /// The token the gate was opened under; the build abandons if it is no
    /// longer the current one.
    pub token: Option<u64>,
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
    pub(crate) fn begin_swap(&self, slot: &SessionSlot) -> u64 {
        let token = self.swap_epochs.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        self.swapping.lock().insert(slot.clone(), token);
        token
    }

    /// Whether a swap is in flight on this seat.
    pub(crate) fn swap_in_progress(&self, slot: &SessionSlot) -> bool {
        self.swapping.lock().contains_key(slot)
    }

    /// Whether a handoff owns this seat's composer right now: its judgment
    /// turn is running, or the swap it opened is still building or spawning.
    ///
    /// **The domain's own flag is cleared at the swap**, before the successor
    /// connects - so a page attaching in that window would otherwise read a
    /// live composer over a seat that has no session at all.
    pub fn handoff_in_flight(&self, slot: &SessionSlot) -> bool {
        self.swap_in_progress(slot)
            || self.domain_session_for(slot).is_some_and(|domain| domain.lock().handoff.awaiting)
    }

    /// End the machine turn a handoff raised, wherever its build stops.
    ///
    /// **Every path out of the build that leaves no successor needs this.**
    /// The frame at the handoff's begin is the live page's only source, and a
    /// successor's record read is what normally clears it - so a build that
    /// stops without one would leave the seat's composer down for good.
    fn end_machine_turn(&self, slot: &SessionSlot) {
        let _ =
            self.update_tx.send(SessionUpdate::MachineTurn { key: slot.clone(), machine: None });
    }

    /// Whether `token` still owns the seat's swap.
    ///
    /// **A build that lost the seat must not spawn.** A replacement during
    /// `ground_truth` closes the gate, and a later handoff may open a new
    /// one; only the token says whether the NewSession this build is about
    /// to dispatch is still its own.
    pub(crate) fn swap_is_current(&self, slot: &SessionSlot, token: u64) -> bool {
        self.swapping.lock().get(slot) == Some(&token)
    }

    /// Dispatch a peer message's prompt, holding it for the successor when a
    /// swap is in flight and keeping its sender so a handoff that fails can
    /// answer it.
    pub(crate) fn dispatch_peer_prompt_under(
        self: &Arc<Self>,
        caller: &SessionSlot,
        key: &SessionSlot,
        text: String,
        uuid: String,
    ) -> Result<(), crate::DispatchError> {
        let held = HeldPrompt {
            text: text.clone(),
            attachments: Vec::new(),
            source: crate::protocol::PromptSource::Peer,
            sender: Some(caller.clone()),
        };
        if self.hold_prompt(key, held) {
            return Ok(());
        }
        self.route(Command::PromptUnder {
            key: key.clone(),
            text,
            attachments: Vec::new(),
            uuid,
            source: crate::protocol::PromptSource::Peer,
        })
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

    /// The handoff row a seat's OCCUPANT arrived by, read from the store.
    ///
    /// This is what a view attaching after the swap - or after a forge
    /// restart - draws the row from, where the transport holds no live copy.
    /// It is looked up by the occupant, not by recency: a later handoff that
    /// never reached its successor must not hide the row of the session that
    /// is actually running.
    pub fn handoff_record_for(&self, slot: &SessionSlot, occupant: &str) -> Option<HandoffRecord> {
        let guard = self.db.lock();
        let db = guard.as_ref()?;
        match crate::store::handoffs::for_successor(db, slot, occupant) {
            Ok(found) => found.map(|(record, _)| record),
            Err(error) => {
                tracing::warn!(
                    target: "forge_workspace::handoff",
                    event_name = "handoff_record_unreadable",
                    slot = %slot.display(),
                    %error,
                    "the seat's handoff record could not be read",
                );
                None
            }
        }
    }

    /// The brief waiting for `slot`'s successor, when one is.
    pub(crate) fn pending_brief_for(&self, slot: &SessionSlot) -> Option<PendingBrief> {
        self.pending_handoffs.lock().get(slot).cloned()
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
            let Some(counts) = successor_measurement(&messages, &predecessor_reads) else {
                tracing::debug!(
                    target: "forge_workspace::handoff",
                    event_name = "handoff_successor_measurement_skipped",
                    slot = %slot.display(),
                    "the successor's transcript read answered nothing; no counts are written",
                );
                return;
            };
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
        // **The gate was opened at the decision, and the token says whether
        // it is still this build's.** A replacement during the read below
        // closes it, and a later handoff may open a new one; this build must
        // not then spawn a second successor.
        let Some(token) = request.token else {
            tracing::debug!(
                target: "forge_workspace::handoff",
                event_name = "handoff_build_without_token",
                slot = %request.slot.display(),
                "a handoff build had no swap token; it is not run",
            );
            self.end_machine_turn(&request.slot);
            return;
        };
        if !self.swap_is_current(&request.slot, token) {
            tracing::debug!(
                target: "forge_workspace::handoff",
                event_name = "handoff_build_lost_the_seat",
                slot = %request.slot.display(),
                "the swap gate closed before the build began; the handoff is not run",
            );
            self.end_machine_turn(&request.slot);
            return;
        }
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
        // **The predecessor's own brief is where its original ask survives.**
        // A session that itself arrived by a handoff has the prior brief as
        // its first transcript row, not the words a person typed; the record
        // it arrived by still holds them.
        let prior =
            self.handoff_for_successor(&request.slot, &request.from).map(|(record, _)| record);
        let live = {
            let domain = self.domain_session_for(&request.slot);
            match domain {
                Some(domain) => live_work_sections(Some(&domain.lock())),
                None => live_work_sections(None),
            }
        };
        let truth = ground_truth(self.config_dir(), &seat, &board, prior.as_ref(), live).await;
        let GroundTruth { sections, cycle, reads, judgment_turn } = truth;
        let record = HandoffRecord {
            slot: request.slot.clone(),
            from: request.from.clone(),
            to: request.to.clone(),
            reason: request.reason,
            fill_percent: request.fill.map(|fill| fill.percent),
            total_tokens: request.fill.map(|fill| fill.total_tokens),
            max_tokens: request.fill.map(|fill| fill.max_tokens),
            at_percent: request.at_percent,
            brief: HandoffBrief { ground_truth: sections, judgment: request.judgment },
            judgment_turn,
            transcript_path: transcript_path.display().to_string(),
            at: crate::install::rfc3339_now(),
            cycle,
            successor: None,
        };
        let text = brief_text(&record.pair(), &record.brief);
        // **The seat is checked again with the build in hand.** The read and
        // the git probe take seconds, and a replacement in that stretch
        // means this successor is no longer the seat's: no row, no brief and
        // no spawn.
        if !self.swap_is_current(&request.slot, token) {
            tracing::debug!(
                target: "forge_workspace::handoff",
                event_name = "handoff_build_lost_the_seat",
                slot = %request.slot.display(),
                "the swap gate closed while the brief was built; the handoff is not run",
            );
            self.end_machine_turn(&request.slot);
            return;
        }
        self.record_handoff(&record, &reads);
        // The brief is held before the row and the spawn, so a successor
        // that connects immediately cannot outrun it.
        self.set_pending_brief(&request.slot, PendingBrief { to: request.to.clone(), text });
        let _ = self
            .update_tx
            .send(SessionUpdate::Handoff { key: request.slot.clone(), handoff: record });
        // **A swap is a replacement**: the previous occupant's tabs close
        // before its successor starts, or the new occupant's first call would
        // inherit them. A parked hand-off's tab is the fleet's to keep.
        self.replace_browser_seat(&request.slot);
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
            for prompt in &held {
                if let Some(sender) = &prompt.sender {
                    self.notice_undelivered_message(
                        sender,
                        &request.slot,
                        crate::mcp::peers::types::PeerFailureReason::TargetConnectionFailed,
                    );
                } else {
                    let first = prompt.text.lines().next().unwrap_or_default();
                    let _ = self.update_tx.send(SessionUpdate::Notice {
                        key: request.slot.clone(),
                        severity: NoticeSeverity::Error,
                        text: format!("a prompt held for the failed handoff did not run: {first}"),
                    });
                }
            }
            let _ = self.update_tx.send(SessionUpdate::Notice {
                key: request.slot.clone(),
                severity: NoticeSeverity::Error,
                text: format!("the handoff could not start a new session: {error}"),
            });
            self.end_machine_turn(&request.slot);
        }
    }
}

/// A fresh session id, deliberately NOT recorded against a slot's row.
///
/// **A handoff records its successor on Connected, not before the spawn.**
/// Minting-and-recording ahead of a spawn that then fails would leave the
/// next boot resuming an id that never ran, which an empty session then fills
/// and a handoff record claims to have joined.
pub(crate) fn mint_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
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
    ///
    /// **The seat's facts are one labelled row each.** The brief draws a
    /// table, so a fact carried inside another row's value has no column of
    /// its own and reads as part of the label above it.
    #[test]
    fn the_seat_and_the_record_name_the_identity_and_the_transcript() {
        let ground =
            assemble(&seat(), &[], &HandoffGitState::default(), &[], None, &[], Vec::new());
        let labels: Vec<&str> = ground.sections.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(
            &labels[..6],
            ["seat", "cwd", "branch", "account", "model", "mode"],
            "the seat block is one fact per row: {labels:?}",
        );
        let values: Vec<Vec<&str>> = ground.sections[..6]
            .iter()
            .map(|section| section.lines.iter().map(String::as_str).collect())
            .collect();
        assert_eq!(
            values,
            vec![
                vec!["Busytools/forge/handoff-cut"],
                vec!["/tmp/forge"],
                vec!["not a git repository"],
                vec!["Personal"],
                vec!["claude-opus-5-5"],
                vec!["auto"],
            ],
            "each row carries its own value",
        );
        let git = HandoffGitState {
            branch: Some("worktree-handoff-cut".to_owned()),
            head: Some("94cab2f20".to_owned()),
            ..HandoffGitState::default()
        };
        let on_a_branch = assemble(&seat(), &[], &git, &[], None, &[], Vec::new());
        assert_eq!(
            on_a_branch.sections[2].label, "branch",
            "the branch keeps its own row: {:?}",
            on_a_branch.sections[2],
        );
        assert_eq!(
            on_a_branch.sections[2].lines,
            vec!["worktree-handoff-cut @ 94cab2f20".to_owned()],
            "and names the commit it is at",
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

    /// **Forge's own prompts are not the person's.** A forged row reads back
    /// as a user turn, so without the filter the judgment prompt becomes the
    /// newest ask and the previous brief becomes the first one.
    #[test]
    fn forge_prompts_and_prior_briefs_are_not_the_asks() {
        let forged = |text: &str| {
            let mut frame = user(text);
            if let Message::User { uuid, .. } = &mut frame {
                *uuid = Some("forge_nudge_7_deadbeef".to_owned());
            }
            frame
        };
        let messages = vec![
            user("ship the handoff"),
            forged("A handoff is starting for this session: its context is nearly full"),
            user("and open the PR"),
            user("HANDOFF - old -> new\n\nPART 1: GROUND TRUTH"),
        ];
        let asks = user_prompts(&messages);
        assert_eq!(
            asks.iter().map(|(_, text)| text.as_str()).collect::<Vec<_>>(),
            vec!["ship the handoff", "and open the PR"],
            "only the two real prompts are asks",
        );
        let constraints = constraint_section(&asks);
        assert!(
            !constraints.lines.iter().any(|line| line.contains("restate what is on disk")),
            "the judgment prompt's own instruction is not a user constraint: {:?}",
            constraints.lines,
        );
    }

    /// The CLI's own user rows are not asks, and they are not turns a
    /// successor is measured by either.
    #[test]
    fn machine_rows_are_neither_asks_nor_successor_turns() {
        let messages = vec![
            user("ship the handoff"),
            user("<task-notification>\n<task-id>b1</task-id>\n</task-notification>"),
            user("[Request interrupted by user for tool use]"),
            user("[Your previous response had no visible output. Please continue.]"),
        ];
        let asks = user_prompts(&messages);
        assert_eq!(
            asks.iter().map(|(_, text)| text.as_str()).collect::<Vec<_>>(),
            vec!["ship the handoff"],
            "only the person's own row is an ask",
        );
        let counts = successor_counts(&messages, &[]);
        assert_eq!(counts.turns, 1, "and only that row is a successor turn: {counts:?}");
    }

    /// A successor that hands over again still has its predecessor's ask and
    /// rules: they live in the record it arrived by, not in the transcript
    /// above it (whose first row is the brief).
    #[test]
    fn a_second_handoff_carries_the_original_ask_and_rules() {
        let prior = HandoffRecord {
            slot: SessionSlot::worker("Busytools", "forge", "handoff-cut"),
            from: "older-session".to_owned(),
            to: "mid-session".to_owned(),
            reason: HandoffReason::Fill,
            fill_percent: Some(85),
            total_tokens: Some(850_000),
            max_tokens: Some(1_000_000),
            at_percent: 85,
            brief: HandoffBrief {
                ground_truth: vec![
                    HandoffSection {
                        label: "ask".to_owned(),
                        lines: vec!["first (user prompt 1): build the handoff row".to_owned()],
                    },
                    HandoffSection {
                        label: "constraints".to_owned(),
                        lines: vec!["\"Never drop a row\" (user prompt 1)".to_owned()],
                    },
                ],
                judgment: Vec::new(),
            },
            judgment_turn: Vec::new(),
            transcript_path: "/tmp/older.jsonl".to_owned(),
            at: "2026-10-24T08:00:00Z".to_owned(),
            cycle: HandoffCycle::default(),
            successor: None,
        };
        let messages = vec![
            user("HANDOFF - older-session -> mid-session\n\nPART 1: GROUND TRUTH"),
            user("now finish the client row"),
        ];

        let ground = assemble(
            &seat(),
            &messages,
            &HandoffGitState::default(),
            &[],
            Some(&prior),
            &[],
            Vec::new(),
        );
        let ask = ground.sections.iter().find(|section| section.label == "ask").expect("ask");
        assert!(
            ask.lines.iter().any(|line| line.contains("build the handoff row")),
            "the original ask is carried: {:?}",
            ask.lines,
        );
        assert!(
            ask.lines.iter().any(|line| line.contains("now finish the client row")),
            "and the current one is added below it: {:?}",
            ask.lines,
        );
        let constraints = ground
            .sections
            .iter()
            .find(|section| section.label == "constraints")
            .expect("constraints");
        assert!(
            constraints.lines.iter().any(|line| line.contains("Never drop a row")),
            "the rule outlives the session that stated it: {:?}",
            constraints.lines,
        );
    }

    /// A peer's words are labelled as the peer's, and the envelope header is
    /// not quoted back as its body.
    #[test]
    fn a_peer_message_is_not_quoted_as_the_users_prompt() {
        let asks = user_prompts(&[user(
            "[Message id=m-1 from agent 'forge/steward' (org 'Busytools')]\n\nis the cron filed?",
        )]);
        let section = ask_section(&asks);
        let line = section.lines.first().expect("the ask");
        assert!(
            line.contains("peer message from forge/steward (org Busytools)"),
            "the author is named: {line}",
        );
        assert!(!line.contains("[Message id="), "and the envelope is not the body: {line}");
    }

    /// A heading block past the cap says how much it left: a rule read as
    /// five bullets when ten were written is a shortened rule presented as
    /// the whole.
    #[test]
    fn a_heading_block_states_what_it_cut() {
        let bullets = (1..=8).map(|n| format!("- rule {n}")).collect::<Vec<_>>().join("\n");
        // The indented line sits directly under the heading: raw indentation,
        // not the trimmed text, is what makes it part of the rule.
        let prompts = user_prompts(&[user(&format!(
            "Do the thing.\nHard rule:\n  the wrapped tail\n{bullets}"
        ))]);
        let section = constraint_section(&prompts);
        let quoted = section.lines.first().expect("the rule");
        assert!(quoted.contains("rule 1"), "the block is quoted: {quoted}");
        assert!(
            quoted.contains("the wrapped tail"),
            "an indented line under the heading is part of it: {quoted}",
        );
        assert!(quoted.contains("[cut; 4 more lines]"), "the omitted rules are counted: {quoted}");
    }

    /// A gate with no result in the transcript is unknown, never a pass.
    #[test]
    fn a_gate_with_no_recorded_result_is_not_a_pass() {
        let messages = vec![call("g1", "Bash", serde_json::json!({ "command": "just check" }))];
        let section = verify_section(&messages);
        assert!(
            section.lines.contains(&"gate: just check -> no result recorded".to_owned()),
            "an unanswered gate is stated as unknown: {:?}",
            section.lines,
        );
        assert!(
            !section.lines.iter().any(|line| line.ends_with("-> ok")),
            "and it is never read as a pass",
        );
    }

    /// The uncommitted count is the tree's, not the capped list's.
    #[test]
    fn the_uncommitted_count_is_the_trees_not_the_caps() {
        let git = HandoffGitState {
            branch: Some("main".to_owned()),
            head: Some("abc".to_owned()),
            dirty: (0..20).map(|n| format!(" M file{n}.rs")).collect(),
            dirty_total: 60,
            diff: Vec::new(),
        };
        let section = files_section(&[], &git, &[]);
        assert!(
            section
                .lines
                .iter()
                .any(|line| line.contains("uncommitted: 60 paths, newest 20 shown (40 omitted)")),
            "the line states the total and what it omits: {:?}",
            section.lines,
        );
    }

    /// A heading constraint takes the block under it: `Hard rule:` alone is
    /// a label, and the bullets are the rule.
    #[test]
    fn a_heading_constraint_takes_its_block() {
        let prompts = user_prompts(&[user(
            "Please do the thing.\nHard rule:\n- Edit/Write only, never sed -i.\n- Never touch main.",
        )]);
        let section = constraint_section(&prompts);
        let quoted = section.lines.first().expect("the rule");
        assert!(
            quoted.contains("Hard rule:")
                && quoted.contains("Edit/Write only")
                && quoted.contains("Never touch main"),
            "the heading and both bullets are quoted: {quoted}",
        );
    }

    /// A clipped verbatim quote says it was cut: a brief calling a quote
    /// verbatim must not present a shortened one as the whole.
    #[test]
    fn a_clipped_quote_states_the_cut() {
        let long = format!("Hard rule: {}", "x".repeat(CONSTRAINT_LINE_CAP));
        let prompts = user_prompts(&[user(&long)]);
        let section = constraint_section(&prompts);
        assert!(
            section
                .lines
                .first()
                .is_some_and(|line| line.contains("[cut; the full text is in the transcript]")),
            "the cut is named: {:?}",
            section.lines,
        );
    }

    /// The brief carries the last three prompts and nothing older. A long
    /// session's opener is not the intent by the time prompts 93-95 arrive,
    /// so quoting it beside them states a starting point the work left.
    #[test]
    fn the_ask_names_the_newest_three() {
        let messages: Vec<Message> =
            ["one", "two", "three", "four", "five"].iter().map(|text| user(text)).collect();
        let asks = user_prompts(&messages);
        let section = ask_section(&asks);
        assert_eq!(
            section.lines,
            vec![
                "#3 (user prompt 3): three".to_owned(),
                "#4 (user prompt 4): four".to_owned(),
                "#5 (user prompt 5): five".to_owned(),
            ],
            "the newest three, oldest first, with no first-prompt line",
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
            dirty_total: 1,
            diff: vec![" src/handoff.rs | 10 +++++-----".to_owned()],
        };
        let section = files_section(&messages, &git, &[]);
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

    /// **The session's own actions were invisible to the brief.** A successor
    /// inheriting an MCP call it cannot see repeats it or undoes it, so the
    /// ground truth names every forge action the session took, one line each -
    /// and leaves the file and command families to the sections above them.
    #[test]
    fn the_brief_lists_the_sessions_forge_actions() {
        let messages = vec![
            call(
                "a1",
                "mcp__forge__agents__send_message",
                serde_json::json!({
                    "org": "Busytools", "project": "forge", "label": "lead",
                    "message": "the handoff is ready"
                }),
            ),
            call(
                "a2",
                "mcp__forge__tasks__create",
                serde_json::json!({ "subject": "draw the provisional row" }),
            ),
            call("a3", "mcp__forge__gotify__apps", serde_json::json!({})),
            call("f1", "Read", serde_json::json!({ "file_path": "src/lib.rs" })),
            call("f2", "Bash", serde_json::json!({ "command": "just check" })),
        ];
        let ground =
            assemble(&seat(), &messages, &HandoffGitState::default(), &[], None, &[], Vec::new());
        let actions = ground
            .sections
            .iter()
            .find(|section| section.label == "actions")
            .expect("the brief carries the session's actions");
        assert!(
            actions.lines.iter().any(|line| line.contains("agents__send_message")
                && line.contains("the handoff is ready")),
            "the message it sent is named: {:?}",
            actions.lines,
        );
        assert!(
            actions
                .lines
                .iter()
                .any(|line| line.contains("tasks__create")
                    && line.contains("draw the provisional row")),
            "and the row it filed: {:?}",
            actions.lines,
        );
        assert!(
            actions.lines.iter().any(|line| line == "gotify__apps"),
            "every forge family is named, the connector's included: {:?}",
            actions.lines,
        );
        assert!(
            !actions.lines.iter().any(|line| line.starts_with("Read") || line.starts_with("Bash")),
            "the file and command families stay in their own sections: {:?}",
            actions.lines,
        );

        let quiet = assemble(&seat(), &[], &HandoffGitState::default(), &[], None, &[], Vec::new());
        assert!(
            quiet.sections.iter().all(|section| section.label != "actions"),
            "a session with no forge-tool calls gets no blank heading: {:?}",
            quiet.sections.iter().map(|section| section.label.as_str()).collect::<Vec<_>>(),
        );
    }

    /// A long list keeps the first entry and the newest, and says what it
    /// left out: the first file a successor changed is the one it will
    /// re-read, so a newest-only cut loses the wrong one.
    #[test]
    fn a_long_file_list_keeps_the_first_and_the_newest() {
        let messages: Vec<Message> = (0..12)
            .map(|n| {
                call(
                    &format!("t{n}"),
                    "Edit",
                    serde_json::json!({ "file_path": format!("src/f{n}.rs") }),
                )
            })
            .collect();
        let section = files_section(&messages, &HandoffGitState::default(), &[]);
        let line = section.lines.first().expect("the changed line");
        assert!(line.contains("10 shown of 12 (2 omitted)"), "the omission is stated: {line}");
        assert!(line.contains("src/f0.rs"), "the first is kept: {line}");
        assert!(line.contains("src/f11.rs"), "and the newest: {line}");
        assert!(!line.contains("src/f1.rs"), "the second-oldest is what the cap takes: {line}");
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

    /// **PART 1 carries what is still running.** The successor has to know
    /// what to resume; the note is an index and may mention it, but the live
    /// set is a fact forge reads itself.
    #[test]
    fn the_live_work_sections_list_agents_background_monitors_and_processes() {
        let mut domain = crate::domain_session::DomainSession::new(
            SessionSlot::worker("Busytools", "forge", "handoff-cut"),
            None,
        );
        domain.card_tracker.apply(&assistant(
            vec![ContentBlock::ToolUse {
                id: "t1".to_owned(),
                name: "Task".to_owned(),
                input: serde_json::json!({
                    "description": "scan the pile",
                    "subagent_type": "Explore",
                    "prompt": "scan",
                }),
                extras: Extras::new(),
            }],
            None,
        ));
        domain.background_tasks = vec![forge_primitives::background::BackgroundTask {
            task_id: "b1".to_owned(),
            is_backgrounded: true,
            task_type: "local_bash".to_owned(),
            description: "cargo watch".to_owned(),
            command: Some("cargo watch".to_owned()),
            tool_use_id: None,
        }];
        domain.monitors = vec![forge_primitives::runtime::MonitorRecord {
            tool_use_id: "m1".to_owned(),
            task_id: Some("mt1".to_owned()),
            description: "watch the build".to_owned(),
            command: "tail -f build.log".to_owned(),
            persistent: false,
            timeout_ms: 0,
            status: forge_primitives::runtime::MonitorStatus::Running,
            output_file: None,
            ended_at: None,
        }];
        domain.process_snapshot = Some(forge_agent::env::processes::ProcessSnapshot {
            processes: vec![forge_agent::env::processes::ProcessEntry {
                pid: 42,
                parent_pid: 1,
                name: "cargo".to_owned(),
                command: "cargo nextest run".to_owned(),
                memory_bytes: 1024,
            }],
            scanned_at: SystemTime::UNIX_EPOCH,
        });

        let sections = live_work_sections(Some(&domain));
        let labels: Vec<&str> = sections.iter().map(|section| section.label.as_str()).collect();
        assert_eq!(labels, ["agents", "background", "monitors", "processes"]);
        assert!(
            sections[0].lines[0].contains("Explore: scan the pile"),
            "the live sub-agent: {:?}",
            sections[0].lines,
        );
        assert!(
            sections[1].lines[0].contains("local_bash: cargo watch [b1]"),
            "the background task: {:?}",
            sections[1].lines,
        );
        assert!(
            sections[2].lines[0].contains("watch the build [m1]"),
            "the monitor: {:?}",
            sections[2].lines,
        );
        assert!(
            sections[3].lines[0].contains("42 cargo: cargo nextest run"),
            "the process: {:?}",
            sections[3].lines,
        );

        let fresh = crate::domain_session::DomainSession::new(
            SessionSlot::worker("Busytools", "forge", "handoff-cut"),
            None,
        );
        let idle = live_work_sections(Some(&fresh));
        assert_eq!(idle[0].lines, vec!["none running".to_owned()]);
        assert_eq!(idle[3].lines, vec!["not scanned".to_owned()], "an absent scan is stated");

        // **Settled work is not live.** A finished sub-agent and a stopped
        // monitor must not list as still running.
        let mut settled = crate::domain_session::DomainSession::new(
            SessionSlot::worker("Busytools", "forge", "handoff-cut"),
            None,
        );
        settled.card_tracker.apply(&assistant(
            vec![ContentBlock::ToolUse {
                id: "t1".to_owned(),
                name: "Task".to_owned(),
                input: serde_json::json!({ "description": "scan the pile", "prompt": "scan" }),
                extras: Extras::new(),
            }],
            None,
        ));
        settled.card_tracker.apply(&result("t1", "done", false));
        settled.monitors = vec![forge_primitives::runtime::MonitorRecord {
            tool_use_id: "m2".to_owned(),
            task_id: Some("mt2".to_owned()),
            description: "watch the build".to_owned(),
            command: "tail -f build.log".to_owned(),
            persistent: false,
            timeout_ms: 0,
            status: forge_primitives::runtime::MonitorStatus::Stopped,
            output_file: None,
            ended_at: None,
        }];
        let done = live_work_sections(Some(&settled));
        assert_eq!(
            done[0].lines,
            vec!["none running".to_owned()],
            "a finished sub-agent is not live",
        );
        let stopped = "a stopped monitor is not live";
        assert_eq!(done[2].lines, vec!["none running".to_owned()], "{stopped}");

        let absent = live_work_sections(None);
        assert_eq!(absent.len(), 4);
        for section in &absent {
            assert_eq!(
                section.lines,
                vec!["not read".to_owned()],
                "an absent domain is stated as unread, not as nothing running",
            );
        }
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
        let raw = "Now: the cut is half in.\nWhere it stood: worktree at abc1234.\nPointers: memory/handoff.md\nNext: the client row.\nDone means: the gates are green.";
        let sections = parse_judgment(raw);
        let labels: Vec<&str> = sections.iter().map(|section| section.label.as_str()).collect();
        assert_eq!(labels, JUDGMENT_HEADINGS.to_vec(), "the five, in the order asked for");
        assert_eq!(sections[1].lines, vec!["worktree at abc1234.".to_owned()]);
    }

    /// **Every spelling of a heading line the model might write parses.**
    /// A note written under `## Now` used to arrive as the missing-note line,
    /// so the bare, hashed and emphasised forms all have to land - alone on
    /// the line, and with its colon and body.
    #[test]
    fn every_heading_spelling_parses() {
        for (label, body) in [
            ("Now", "the cut is half in."),
            ("Where it stood", "worktree handoff-index-first at abc1234, no PR yet."),
            ("Pointers", "- memory/handoff.md"),
            ("Next", "the client row."),
            ("Done means", "the gates are green."),
        ] {
            for line in [
                format!("{label}: {body}"),
                format!("## {label}: {body}"),
                format!("**{label}:** {body}"),
                format!("- **{label}:** {body}"),
            ] {
                let sections = parse_judgment(&line);
                assert_eq!(sections.len(), 1, "one section for {line}");
                assert_eq!(sections[0].label, label, "the heading for {line}");
                assert_eq!(sections[0].lines, vec![body.to_owned()], "its body for {line}");
            }
            // A heading alone on its line takes the body from the lines
            // below, including the numbered and punctuated spellings the
            // shipped skill invites by numbering its own parts.
            for line in [
                format!("## {label}"),
                format!("**{label}**"),
                label.to_owned(),
                format!("## {label}."),
                format!("1. **{label}**"),
                format!("## 1. {label}"),
            ] {
                let sections = parse_judgment(&format!("{line}\n{body}"));
                assert_eq!(sections.len(), 1, "one section for {line}");
                assert_eq!(sections[0].label, label, "the heading for {line}");
                assert_eq!(
                    sections[0].lines,
                    vec![body.to_owned()],
                    "the body from the line below for {line}",
                );
            }
        }
    }

    /// **The label's own emphasis comes off; the body's stays.** Stripping a
    /// leading asterisk run rather than the matched pair left a note reading
    /// `nothing*` where its author wrote `*nothing*`.
    #[test]
    fn a_heading_strips_its_own_emphasis_and_leaves_the_body_s() {
        let sections = parse_judgment("**Now:** *nothing* has changed.");
        assert_eq!(sections.len(), 1, "one section");
        assert_eq!(sections[0].label, "Now");
        assert_eq!(sections[0].lines, vec!["*nothing* has changed.".to_owned()]);
    }

    /// **A note written in markdown is the judgment half.** The regression
    /// this fixes: a correctly written `## Now` note read as no note at all,
    /// and the successor was told the note had not arrived.
    #[test]
    fn a_markdown_note_arrives_as_the_judgment_half() {
        let note = "\
## Now
The cut is half in and the parser is next.
## Where it stood
worktree handoff-index-first at abc1234, no PR yet.
## Pointers
- memory/handoff.md
## Next
fix the parser, then the client row.
## Done means
both gates green.";
        assert!(parses_as_note(note), "the note is recognised");
        let sections = parse_judgment(note);
        assert_eq!(
            sections.iter().map(|section| section.label.as_str()).collect::<Vec<_>>(),
            JUDGMENT_HEADINGS.to_vec(),
            "the five, in order",
        );
        assert_eq!(
            sections[0].lines,
            vec!["The cut is half in and the parser is next.".to_owned()],
            "the body under the heading",
        );
    }

    /// **A note with none of the five headings is not the brief.** The caller
    /// answers exactly this with `missing_note()`, so a note under headings
    /// that are no longer asked for must not read as one.
    #[test]
    fn a_note_without_a_recognised_heading_is_not_the_brief() {
        assert!(
            !parses_as_note("Tried and failed: the first spawn.\nDo not: delete the row."),
            "the old headings are not the shape any more",
        );
        assert!(!parses_as_note("## Summary\nsome prose"), "nor is a markdown heading of its own");
        assert!(
            !parses_as_note("as I said, Now: the work stands and Next: nothing else."),
            "a heading has to open its line, not appear inside one",
        );
        assert!(parses_as_note("**Next:** the client row."), "an emphasised heading still is");
    }

    /// **The cap states its cut.** A note past the ceiling loses its tail,
    /// and the row says so rather than presenting what is left as the whole
    /// of what the old session wrote.
    #[test]
    fn a_note_past_the_cap_states_the_cut() {
        let note = format!("Now: {}\nNext: {}", "x".repeat(JUDGMENT_CAP - 200), "y".repeat(400));
        let sections = parse_judgment(&note);
        assert_eq!(sections.len(), 1, "one section survives the cut: {sections:?}");
        assert_eq!(sections[0].label, "Now", "and it is the note's own front: {sections:?}");
        assert!(
            sections[0].lines.first().is_some_and(|line| line.starts_with("xxx")),
            "the cap keeps the front of the note, not its tail: {sections:?}",
        );
        assert!(
            sections[0]
                .lines
                .last()
                .is_some_and(|line| line.contains("in the transcript named above")),
            "the cut is stated as the kept section's last line: {sections:?}",
        );
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

    /// The brief reaches the successor as a prompt and nowhere else: its
    /// text is already in the handoff row, so a forged conversation row
    /// would draw it twice (and the transcript a third time on reload).
    #[test]
    fn the_brief_does_not_forge_a_second_row() {
        use crate::protocol::PromptSource;

        let (workspace, mut rx) = crate::Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        let slot = SessionSlot::lead("Busytools", "forge");

        workspace
            .dispatch_handoff_brief(&slot, "HANDOFF - a -> b".to_owned(), "brief-1".to_owned())
            .expect("the brief is routed");

        let routed = workspace.drain_test_dispatch_buffer();
        assert!(
            routed.iter().any(|command| matches!(
                command,
                Command::PromptUnder { text, source: PromptSource::Forge, .. }
                    if text.starts_with("HANDOFF - ")
            )),
            "the successor runs it as forge's first turn: {routed:?}",
        );
        while let Ok(frame) = rx.try_recv() {
            assert!(
                !matches!(frame.update, SessionUpdate::ChatAppended { .. }),
                "no conversation row is forged for it: {:?}",
                frame.update,
            );
        }
    }

    /// **A handoff owns the seat from its begin until its successor lands.**
    /// The domain's own flag is cleared at the swap, so the gate is what
    /// carries the build and spawn window - a page attaching there must not
    /// read a live composer over a seat with no session.
    #[test]
    fn a_handoff_is_in_flight_from_its_begin_until_the_swap_closes() {
        let (workspace, _rx) = crate::Workspace::testing_stub();
        let slot = SessionSlot::lead("TestOrg", "forge");
        workspace.register_domain_session(slot.clone(), None);

        assert!(!workspace.handoff_in_flight(&slot), "an idle seat takes input");

        workspace.domain_session_for(&slot).expect("the domain").lock().handoff.awaiting = true;
        assert!(workspace.handoff_in_flight(&slot), "the judgment turn owns it");
        workspace.domain_session_for(&slot).expect("the domain").lock().handoff.awaiting = false;

        workspace.begin_swap(&slot);
        assert!(workspace.handoff_in_flight(&slot), "the swap's build and spawn own it");
        workspace.end_swap(&slot);
        assert!(!workspace.handoff_in_flight(&slot), "the successor hands the seat back");
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

    fn swap_request(slot: &SessionSlot, token: Option<u64>) -> SwapRequest {
        SwapRequest {
            slot: slot.clone(),
            from: "old-session-id".to_owned(),
            to: "new-session-id".to_owned(),
            token,
            reason: forge_primitives::HandoffReason::Manual,
            fill: None,
            at_percent: 85,
            role: Role::Lead,
            account: None,
            model: None,
            permission_mode: None,
            judgment: Vec::new(),
            cwd: "/tmp/handoff-build".to_owned(),
            launch_settings: crate::SessionLaunchSettings::default(),
        }
    }

    /// **A successor replaces the seat's tabs before it starts**: the old
    /// occupant's tabs belong to it, and the new occupant's first call must
    /// not inherit them.
    #[tokio::test]
    async fn a_successor_replaces_the_seats_tabs_before_it_starts() {
        let (workspace, _rx) = crate::Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        let (calls, mut heard) = tokio::sync::mpsc::unbounded_channel();
        workspace
            .install_browser_host(std::sync::Arc::new(crate::browser::testing::StubHost { calls }));
        let slot = SessionSlot::lead("Busytools", "forge");
        let token = workspace.begin_swap(&slot);

        Arc::clone(&workspace).run_handoff(swap_request(&slot, Some(token))).await;

        let (what, args) = tokio::time::timeout(std::time::Duration::from_secs(2), heard.recv())
            .await
            .expect("the fleet hears the replacement")
            .expect("a replacement");
        assert_eq!(what, "replace_seat", "the seat is replaced: {args}");
        assert_eq!(args, serde_json::json!(slot));
        assert!(
            workspace
                .drain_test_dispatch_buffer()
                .iter()
                .any(|command| matches!(command, Command::NewSession { .. })),
            "and the successor is dispatched after it",
        );
    }

    /// A new swap on the seat invalidates the token that opened the last one,
    /// and a closed gate owns nothing.
    #[test]
    fn a_new_swap_invalidates_the_previous_token() {
        let (workspace, _rx) = crate::Workspace::testing_stub();
        let slot = SessionSlot::lead("Busytools", "forge");
        let first = workspace.begin_swap(&slot);
        assert!(workspace.swap_is_current(&slot, first), "the opener owns the gate");

        let second = workspace.begin_swap(&slot);
        assert!(!workspace.swap_is_current(&slot, first), "the replacement owns the seat now");
        assert!(workspace.swap_is_current(&slot, second));

        workspace.end_swap(&slot);
        assert!(!workspace.swap_is_current(&slot, second), "a closed gate owns nothing");
    }

    /// **A build that loses the seat mid-read writes nothing.** The token is
    /// checked again once the transcript and git read are in hand, because a
    /// replacement in that stretch would otherwise draw a row and spawn a
    /// successor for a seat it no longer runs.
    #[tokio::test]
    async fn a_build_that_loses_the_seat_writes_nothing() {
        use std::task::{Context, Poll, Waker};

        let (workspace, mut rx) = crate::Workspace::testing_stub();
        workspace.enable_test_dispatch_intercept();
        let (calls, mut heard) = tokio::sync::mpsc::unbounded_channel();
        workspace
            .install_browser_host(std::sync::Arc::new(crate::browser::testing::StubHost { calls }));
        let slot = SessionSlot::lead("Busytools", "forge");
        let token = workspace.begin_swap(&slot);
        let mut build =
            Box::pin(Arc::clone(&workspace).run_handoff(swap_request(&slot, Some(token))));

        // Drive the build to its first await: the transcript read.
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(
            matches!(build.as_mut().poll(&mut cx), Poll::Pending),
            "precondition: the build suspended on its read",
        );
        // A second swap takes the seat while the first is still reading.
        workspace.begin_swap(&slot);
        build.await;

        assert!(
            !workspace
                .drain_test_dispatch_buffer()
                .iter()
                .any(|command| matches!(command, Command::NewSession { .. })),
            "no successor is spawned for a seat the build no longer owns",
        );
        assert!(
            workspace.pending_brief_for(&slot).is_none(),
            "and no brief is left waiting for a successor that was never started",
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), heard.recv())
                .await
                .is_err(),
            "a lost seat's tabs are not replaced",
        );
        let mut turns = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            assert!(
                !matches!(frame.update, SessionUpdate::Handoff { .. }),
                "no handoff row is drawn for it: {:?}",
                frame.update,
            );
            if let SessionUpdate::MachineTurn { machine, .. } = frame.update {
                turns.push(machine);
            }
        }
        // **The build's own clear.** The begin announced the machine turn and
        // the lost seat spawns no successor whose record read would end it.
        assert_eq!(turns, vec![None], "the lost build hands the machine turn back");
    }

    /// The machine-turn frames a run emitted, in order.
    fn machine_turns(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::update_fanout::SeatUpdate>,
    ) -> Vec<Option<forge_primitives::runtime::MachineTurn>> {
        let mut out = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            if let SessionUpdate::MachineTurn { machine, .. } = frame.update {
                out.push(machine);
            }
        }
        out
    }

    /// **Every build that stops without a successor hands the machine turn
    /// back.** The begin announced it and no successor's record read follows,
    /// so a seat left holding the blocker would be one nobody can type into.
    #[tokio::test]
    async fn a_build_that_never_starts_hands_the_machine_turn_back() {
        let slot = SessionSlot::lead("TestOrg", "forge");

        // No token: the gate was never this build's.
        let (workspace, mut rx) = crate::Workspace::testing_stub();
        Arc::clone(&workspace).run_handoff(swap_request(&slot, None)).await;
        assert_eq!(machine_turns(&mut rx), vec![None], "no token, no machine turn");

        // The seat taken again before the build looks at it.
        let (workspace, mut rx) = crate::Workspace::testing_stub();
        let token = workspace.begin_swap(&slot);
        workspace.begin_swap(&slot);
        Arc::clone(&workspace).run_handoff(swap_request(&slot, Some(token))).await;
        assert_eq!(machine_turns(&mut rx), vec![None], "a seat lost before the build");
    }

    /// **A build whose successor cannot start hands the machine turn back.**
    /// No dispatch intercept means no route for `NewSession`, which is the
    /// failure the begin's own frame would otherwise outlive.
    #[tokio::test]
    async fn a_build_whose_successor_cannot_start_hands_the_machine_turn_back() {
        let (workspace, mut rx) = crate::Workspace::testing_stub();
        let slot = SessionSlot::lead("TestOrg", "forge");
        let token = workspace.begin_swap(&slot);

        Arc::clone(&workspace).run_handoff(swap_request(&slot, Some(token))).await;

        assert_eq!(machine_turns(&mut rx), vec![None], "the successor never started");
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

    /// A transcript read that answered nothing writes no counts: zeros would
    /// read as a successor that re-read nothing, the best possible brief.
    #[test]
    fn an_empty_read_measures_nothing() {
        assert!(
            successor_measurement(&[], &[]).is_none(),
            "an empty read is a failure, not a perfect successor",
        );
        assert!(
            successor_measurement(&[user("carry on")], &[]).is_some(),
            "a read with a turn in it measures",
        );
        assert!(
            successor_measurement(
                &[
                    user("HANDOFF - old -> new\n\nPART 1: GROUND TRUTH"),
                    assistant(Vec::new(), None),
                ],
                &[],
            )
            .is_none(),
            "a read holding only the brief and the assistant's rows measures nothing",
        );
    }

    /// A predecessor transcript that is not there is stated, not read as a
    /// session with nothing in it.
    #[tokio::test]
    async fn an_absent_transcript_is_stated_in_the_ground_truth() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript_path = dir.path().join("not-there.jsonl");
        let seat = HandoffSeat {
            slot: &SessionSlot::worker("Busytools", "forge", "handoff-cut"),
            from: "missing-session",
            cwd: dir.path(),
            transcript_path: &transcript_path,
            account: None,
            model: None,
            permission_mode: None,
            role: Role::Worker { charter_opening: None },
        };
        let ground = ground_truth(dir.path(), &seat, &[], None, Vec::new()).await;
        assert_eq!(ground.sections[0].label, "note", "the failure leads the brief");
        assert!(
            ground.sections[0].lines[0].contains("could not be read"),
            "and says what it is: {:?}",
            ground.sections[0].lines,
        );
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

    /// The threshold is compared on the pair, before the share is rounded:
    /// an 84.6% reading rounds to 85 and must still not fire at 85.
    #[test]
    fn the_threshold_is_compared_before_rounding() {
        let just_under = FillReading { percent: 85, total_tokens: 846_000, max_tokens: 1_000_000 };
        assert!(!just_under.fires(85), "84.6% is under the line");
        let at = FillReading { percent: 85, total_tokens: 850_000, max_tokens: 1_000_000 };
        assert!(at.fires(85), "exactly 85% is at it");
        let share_only = FillReading { percent: 85, total_tokens: 0, max_tokens: 0 };
        assert!(share_only.fires(85), "a share-only reading falls back to its share");
    }

    /// **The turn runs memory first and ends on the exit call.** The prompt
    /// states the order, and the note's pointer has a home under Now.
    #[test]
    fn the_judgment_prompt_runs_memory_then_the_exit() {
        let prompt = judgment_prompt();
        assert!(
            prompt.contains(
                "memory first, then close out what is running, then call `agents__handoff`"
            ),
            "the order is stated: memory, close out, exit",
        );
        assert!(
            prompt.find("Invoke the handoff skill").expect("the skill is named")
                < prompt.find("Write the note").expect("the contract follows"),
            "the skill runs before the note's contract",
        );
        let now = prompt.find("Now: where the work stands").expect("the Now heading");
        let pointer =
            prompt.find("Its last line names what you recorded in memory").expect("the pointer");
        let stood = prompt.find("Where it stood:").expect("the next heading");
        assert!(
            now < pointer && pointer < stood,
            "the pointer sits under Now, before Where it stood",
        );
        assert!(
            prompt.contains("This note is Step 2's handoff document")
                && prompt.contains("pass the same text to `agents__handoff` as `brief`"),
            "the contract names the skill's document as the one note, not a separate file",
        );
    }

    /// **The memory pointer lands under Now, not in a section of its own.**
    /// Text before the first heading becomes a `judgment` section and text
    /// after `Done means` joins that section, so the prompt has to place the
    /// pointer inside a heading for the row to draw it where it belongs.
    #[test]
    fn the_memory_pointer_lands_under_now() {
        let note = "Now: the cut is half in.\nRecorded in memory.md and its index.\n\
                    Where it stood: worktree at abc1234.\nPointers: memory/handoff.md\n\
                    Next: the client row.\nDone means: the gates are green.";
        let sections = parse_judgment(note);
        assert_eq!(
            sections.iter().map(|section| section.label.as_str()).collect::<Vec<_>>(),
            JUDGMENT_HEADINGS.to_vec(),
            "the note is exactly the five, with no section before or after them",
        );
        assert!(
            sections[0].lines.contains(&"Recorded in memory.md and its index.".to_owned()),
            "the pointer rides under Now: {:?}",
            sections[0].lines,
        );
    }

    /// **The five headings are the note's LINES, exactly and in order.** A
    /// presence check passes for an inline mention or a sixth heading; the
    /// row's label column and the parser both read line starts.
    #[test]
    fn the_judgment_prompt_asks_for_exactly_the_five_headings() {
        let prompt = judgment_prompt();
        let labels: Vec<&str> = prompt
            .lines()
            .filter_map(|line| {
                let (label, _) = line.split_once(": ")?;
                let shaped = label.len() <= 21
                    && label.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                    && label.chars().all(|c| c.is_ascii_alphabetic() || c == ' ');
                shaped.then_some(label)
            })
            .collect();
        assert_eq!(
            labels,
            JUDGMENT_HEADINGS.to_vec(),
            "the prompt's heading lines are exactly the five, in the order the row draws",
        );
    }

    /// **The shipped skill is Ved's handoff skill, verbatim, plus the forge
    /// additions.** Nothing above the appended section is rewritten: the
    /// memory step, the handoff document, its four parts and the index line
    /// are his text.
    #[test]
    fn the_shipped_skill_keeps_the_whole_handoff_procedure() {
        // Pinned on the words, not the wrap: the file is prose.
        let flat = SKILL.split_whitespace().collect::<Vec<_>>().join(" ");
        for (clause, why) in [
            ("## Step 1 - Memory first (REQUIRED, before any handoff text)", "his memory step"),
            ("update it in place and delete superseded narration", "in-place updates"),
            (
                "Cross-project patterns or workarounds -> TIL files (`til/`), no need to ask.",
                "TILs",
            ),
            ("Update the memory index (`MEMORY.md`) in the SAME edit", "index in the same edit"),
            ("A learning that exists only in the handoff is not recorded", "memory over handoff"),
            ("The file is `handoff.md` in that directory", "the document he writes"),
            ("1. **State snapshot**", "part 1"),
            ("2. **Next steps / open threads**", "part 2"),
            ("3. **Process facts**", "part 3"),
            ("4. **Suggested skills**", "part 4"),
            ("Redact sensitive information (API keys, passwords, PII).", "redaction"),
            ("SHIPPED / IN FLIGHT / BLOCKED", "the index token"),
            ("## Forge: close out and exit", "the forge addition"),
            ("record in memory what it is and how to resume it", "the close-out step"),
            ("Leave nothing unfinished unrecorded", "unfinished work is named, not dropped"),
            ("pass the same text to `agents__handoff` as `brief`", "the exit call"),
            ("the state snapshot in Now", "part 1 maps onto Now"),
            (
                "the exact position and process facts in Where it stood",
                "parts 1 and 3 map onto Where it stood",
            ),
            ("the paths they live at in Pointers", "the index maps onto Pointers"),
            ("the next steps and open threads in Next", "part 2 maps onto Next"),
            ("the suggested skills in Next", "part 4 maps onto Next"),
        ] {
            let clause = clause.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(flat.contains(&clause), "the shipped skill drops '{clause}' ({why})");
        }
        let names = JUDGMENT_HEADINGS.join(", ");
        let message = format!("the skill names forge's headings, in order: {names}");
        assert!(flat.contains(&names), "{message}");
    }

    /// The judgment turn is what a view draws when it attaches after the
    /// swap, and the transcript it lives in belongs to the session that left.
    /// So the record carries it: from the procedure prompt the handing-over
    /// session ran to the end of its transcript.
    ///
    /// **The last procedure prompt, not the first.** The brief a successor
    /// arrives by is a machinery row too, and it does not open like the
    /// procedure; and an abandoned handoff leaves an earlier prompt behind,
    /// so the closing turn is the newest match.
    #[test]
    fn the_record_carries_the_judgment_turn_and_its_calls() {
        let procedure = |marker: &str| {
            let mut prompt = user(&format!("{MACHINE_PREFIX} {marker}"));
            if let Message::User { uuid, .. } = &mut prompt {
                *uuid = Some(forge_sdk::request_id::next_handoff_prompt_id());
            }
            prompt
        };
        let messages = vec![
            user("ship the handoff"),
            // A handoff that was abandoned leaves its own prompt behind; the
            // turn the record carries is the one that actually handed over.
            procedure("the abandoned one"),
            assistant(vec![text("the handoff was cancelled")], None),
            call("t1", "mcp__forge__memory_write", serde_json::json!({ "path": "/x" })),
            result("t1", "ok", false),
            procedure("the rest of it"),
            assistant(vec![text("Now: done")], None),
            call("t2", "mcp__forge__agents__handoff", serde_json::json!({ "brief": "Now: done" })),
            result("t2", "{\"status\":\"handoff note accepted\"}", false),
        ];
        let ground =
            assemble(&seat(), &messages, &HandoffGitState::default(), &[], None, &[], Vec::new());
        assert_eq!(
            ground.judgment_turn.len(),
            4,
            "the prompt, the call, the answer and the text: {:?}",
            ground.judgment_turn,
        );
        assert!(
            !ground.judgment_turn.iter().any(|message| matches!(
                message,
                Message::User { message, .. }
                    if text_of(&message.content)
                        .is_some_and(|text| text.ends_with("the abandoned one"))
            )),
            "the abandoned handoff's prompt is not the turn: {:?}",
            ground.judgment_turn.first(),
        );
        assert!(
            matches!(
                ground.judgment_turn.first(),
                Some(Message::User { uuid: Some(id), .. })
                    if id.starts_with(forge_sdk::request_id::HANDOFF_PROMPT_PREFIX)
            ),
            "the turn opens on the procedure prompt itself: {:?}",
            ground.judgment_turn.first(),
        );
        assert!(
            !ground.judgment_turn.iter().any(|message| matches!(
                message,
                Message::User { message, .. }
                    if text_of(&message.content).is_some_and(|text| text == "ship the handoff")
            )),
            "the work before the handoff is not part of it",
        );
    }

    /// **The CLI has two carriers for a prompt, and the record must read
    /// both.** A prompt queued behind a running turn lands as a
    /// `queued_command` block with the prompt's id on `source_uuid`; reading
    /// only the row's own words would draw the procedure live and carry no
    /// turn at all on a reload.
    #[test]
    fn a_queued_procedure_prompt_is_the_turn_too() {
        let mut queued = user("");
        if let Message::User { message: envelope, .. } = &mut queued {
            envelope.content = vec![ContentBlock::QueuedCommand {
                prompt: serde_json::Value::String(format!("{MACHINE_PREFIX} queued")),
                command_mode: Some("prompt".to_owned()),
                source_uuid: Some(forge_sdk::request_id::next_handoff_prompt_id()),
                extras: Extras::new(),
            }];
        }
        let messages = vec![
            user("ship the handoff"),
            queued,
            call("t2", "mcp__forge__agents__handoff", serde_json::json!({ "brief": "Now: done" })),
            result("t2", "ok", false),
        ];
        let ground =
            assemble(&seat(), &messages, &HandoffGitState::default(), &[], None, &[], Vec::new());
        assert_eq!(
            ground.judgment_turn.len(),
            3,
            "the queued prompt, the call and its answer: {:?}",
            ground.judgment_turn,
        );
        assert!(
            matches!(
                ground.judgment_turn.first(),
                Some(Message::User { message, .. })
                    if message.content.iter().any(|block| matches!(
                        block,
                        ContentBlock::QueuedCommand { source_uuid: Some(id), .. }
                            if id.starts_with(forge_sdk::request_id::HANDOFF_PROMPT_PREFIX)
                    ))
            ),
            "and the turn opens on that carrier: {:?}",
            ground.judgment_turn.first(),
        );
    }

    /// **The carried turn is capped, and the cut is stated.** It rides every
    /// seat read and every stored row; a procedure that read a large file must
    /// not make the record megabytes, and a shortened turn must not read as
    /// the whole of what the old session did.
    #[test]
    fn a_turn_past_the_cap_keeps_its_prompt_and_states_the_cut() {
        let mut prompt = user(&format!("{MACHINE_PREFIX} the rest of it"));
        if let Message::User { uuid, .. } = &mut prompt {
            *uuid = Some(forge_sdk::request_id::next_handoff_prompt_id());
        }
        let messages = vec![
            user("ship it"),
            prompt.clone(),
            assistant(vec![text("Now: the first thing")], None),
            assistant(vec![text(&"x".repeat(JUDGMENT_TURN_CAP))], None),
            assistant(vec![text("Now: the last thing")], None),
        ];
        let ground =
            assemble(&seat(), &messages, &HandoffGitState::default(), &[], None, &[], Vec::new());
        let turn = &ground.judgment_turn;
        assert!(turn.len() >= 2, "the prompt, the omission and what fitted: {}", turn.len());
        assert_eq!(turn.first(), Some(&prompt), "the prompt is kept whole");
        assert!(
            matches!(turn.last(), Some(Message::System { subtype, data, .. })
                if subtype == "forge_notice"
                    && data.get("uuid").and_then(serde_json::Value::as_str)
                        == Some(HANDOFF_TURN_CUT_ID)
                    && data
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|text| text.contains("[cut;") && text.contains("earlier messages"))),
            "the turn ends on the stated cut, with the id that keeps it one line: {:?}",
            turn.last(),
        );
        assert!(
            serde_json::to_string(turn).map_or(usize::MAX, |json| json.len()) <= JUDGMENT_TURN_CAP,
            "and the carried array, separators included, stays at the ceiling",
        );
    }

    /// **A turn that never ran is not an empty turn.** A handover whose
    /// transcript carries no procedure prompt - a read that failed, or an
    /// exit call with no turn behind it - must carry nothing rather than a
    /// stub row every seat that arrived by it would draw.
    #[test]
    fn a_handoff_with_no_procedure_prompt_carries_no_turn() {
        let messages = vec![user("ship it"), assistant(vec![text("done")], None)];
        let ground =
            assemble(&seat(), &messages, &HandoffGitState::default(), &[], None, &[], Vec::new());
        assert!(ground.judgment_turn.is_empty(), "no prompt, no turn: {:?}", ground.judgment_turn);
    }

    /// `is_machine_prompt` recognizes forge's own turn by this exact prefix;
    /// a reworded opening would have the judgment prompt read back as the
    /// newest ask, and its own instructions as a user constraint. The matcher
    /// is exercised itself, not only the string it is supposed to match.
    #[test]
    fn the_judgment_prompt_is_a_machine_row() {
        let prompt = judgment_prompt();
        assert!(
            judgment_opening().starts_with(MACHINE_PREFIX),
            "the opening carries the matcher's prefix",
        );
        assert!(
            is_machine_prompt(&user(&prompt)),
            "the matcher recognises the real prompt as forge's own row",
        );
        assert!(
            !prompt.contains("context is nearly full"),
            "the opening is true for /handoff as well as the fill trigger",
        );
    }

    /// The prompt ships to every install and names only forge's own things,
    /// the skill and the tool. It carries no path from one machine's tree.
    #[test]
    fn the_judgment_prompt_names_no_path() {
        let prompt = judgment_prompt();
        assert!(
            prompt.contains("Invoke the handoff skill"),
            "the compiled-in prompt is the real one, not an empty string",
        );
        for (token, why) in [
            ("CLAUDE", "one machine's instruction file"),
            ("MEMORY.md", "one machine's memory index"),
            ("~", "a home-relative path"),
            ("/", "a path at all"),
        ] {
            assert!(!prompt.contains(token), "the judgment prompt names '{token}' ({why})");
        }
    }

    /// **The skill is installed into the config dir, and only rewritten when
    /// it differs.** An unchanged copy is left alone, so its mtime does not
    /// move, and a changed one is replaced with the shipped bytes.
    #[test]
    fn the_handoff_skill_is_installed_only_when_it_differs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("skills").join("handoff").join("SKILL.md");

        assert_eq!(install_skill(dir.path()).expect("install"), SkillInstall::Created);
        assert_eq!(
            std::fs::read(&path).expect("read"),
            SKILL.as_bytes(),
            "byte-equal to the asset"
        );

        // The mtime is the unchanged detector's evidence: no write at all.
        let before = std::fs::metadata(&path).expect("stat").modified().expect("mtime");
        assert_eq!(install_skill(dir.path()).expect("install"), SkillInstall::Unchanged);
        let after = std::fs::metadata(&path).expect("stat").modified().expect("mtime");
        assert_eq!(before, after, "an identical skill is not rewritten");

        // **One byte is a difference.** A length-only or prefix comparator
        // would keep this copy and hand a fresh install an edited skill.
        let mut bytes = std::fs::read(&path).expect("read");
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        std::fs::write(&path, &bytes).expect("one byte off");
        assert_eq!(install_skill(dir.path()).expect("install"), SkillInstall::Replaced);
        assert_eq!(
            std::fs::read(&path).expect("read"),
            SKILL.as_bytes(),
            "replaced with the asset"
        );

        // A directory where the file belongs is a read error, not "absent":
        // the install refuses rather than writing through it.
        std::fs::remove_file(&path).expect("remove");
        std::fs::create_dir(&path).expect("dir in its place");
        assert!(install_skill(dir.path()).is_err(), "an unreadable skill is an error");
    }

    /// The name the prompt invokes is the name the skill declares and the
    /// directory it installs under, so the three cannot drift apart.
    #[test]
    fn the_prompt_invokes_the_skill_by_its_shipped_name() {
        let name = SKILL
            .lines()
            .find_map(|line| line.strip_prefix("name:"))
            .map(str::trim)
            .expect("the skill declares its name");
        assert!(
            judgment_prompt().contains(&format!("Invoke the {name} skill")),
            "the prompt names '{name}', the frontmatter's own name",
        );
        assert_eq!(
            skill_path(Path::new("/tmp/cfg")),
            Path::new("/tmp/cfg").join("skills").join(name).join("SKILL.md"),
            "and the install directory is that name",
        );
    }

    /// **The memory roots are wired into the brief itself.** Passing `&[]` in
    /// `ground_truth` would leave every other test green; this seeds a real
    /// transcript and reads the brief that build produces.
    #[tokio::test]
    async fn the_ground_truth_drops_the_memory_edits_from_the_files_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).expect("project dir");
        let cwd = std::fs::canonicalize(&project).expect("canonical");
        let session = "9cfe315c-aaaa-bbbb-cccc-dddddddddddd";
        let key = forge_agent::userdata::catalog::scan::project_key_for_directory(Some(
            &cwd.to_string_lossy(),
        ));
        let transcript =
            forge_sdk::projects_dir_for(config).join(&key).join(format!("{session}.jsonl"));
        std::fs::create_dir_all(transcript.parent().expect("parent")).expect("projects dir");
        let memory = config.join("projects").join(&key).join("memory").join("note.md");
        let rows = [
            serde_json::json!({
                "type": "user",
                "message": {"role": "user", "content": [{"type": "text", "text": "do the thing"}]},
                "session_id": session,
            }),
            serde_json::json!({
                "type": "assistant",
                "session_id": session,
                "message": {
                    "id": "m1",
                    "role": "assistant",
                    "model": "a-model",
                    "content": [{"type": "tool_use", "id": "t1", "name": "Edit",
                                 "input": {"file_path": memory.to_string_lossy()}}],
                },
            }),
            serde_json::json!({
                "type": "assistant",
                "session_id": session,
                "message": {
                    "id": "m2",
                    "role": "assistant",
                    "model": "a-model",
                    "content": [{"type": "tool_use", "id": "t2", "name": "Edit",
                                 "input": {"file_path": "src/lib.rs"}}],
                },
            }),
        ];
        let body = rows.iter().fold(String::new(), |mut body, row| {
            body.push_str(&row.to_string());
            body.push('\n');
            body
        });
        std::fs::write(&transcript, body).expect("write transcript");

        let seat = HandoffSeat {
            slot: &SessionSlot::worker("Busytools", "forge", "handoff-cut"),
            from: session,
            cwd: &cwd,
            transcript_path: &transcript,
            account: None,
            model: None,
            permission_mode: None,
            role: Role::Worker { charter_opening: None },
        };
        let ground = ground_truth(config, &seat, &[], None, Vec::new()).await;
        let files = ground.sections.iter().find(|section| section.label == "files").expect("files");
        let changed =
            files.lines.iter().find(|line| line.starts_with("changed:")).expect("changed");
        assert!(changed.contains("src/lib.rs"), "the project edit is named: {changed}");
        assert!(!changed.contains("memory"), "and the memory edit is not: {changed}");
    }

    /// **A note is only a note if it parses into sections.** Headings with
    /// nothing under them are an empty shell, not the brief.
    #[test]
    fn a_headings_only_reply_is_not_a_note() {
        assert!(!parses_as_note("Now:\nWhere it stood:\nPointers:\nNext:\nDone means:"));
        assert!(!parses_as_note("Memory updated."));
        assert!(parses_as_note("Now: the cut is in."));
    }

    /// **A note at the length the prompt asks for is not truncated.** A later
    /// lowering of the cap would cut a note that followed the prompt.
    #[test]
    fn a_compliant_note_fits_under_the_cap() {
        // Realistic word lengths: at 7-8 letters a 400-word note is roughly
        // 3,200 characters, so a cap set at the note's own length proves
        // nothing about the headroom a complying session has.
        let words = |n: usize, tag: &str| vec![tag; n].join(" ");
        let note = format!(
            "Now: {}\nWhere it stood: {}\nPointers: {}\nNext: {}\nDone means: {}",
            words(80, "morning"),
            words(80, "abandon"),
            words(80, "careful"),
            words(80, "pending"),
            words(80, "observes"),
        );
        assert!(
            note.split_whitespace().count() >= 400,
            "the fixture is at least the length the prompt asks for",
        );
        assert!(
            note.chars().count() > 3_000,
            "and long enough that a materially lower ceiling would cut it: {} chars",
            note.chars().count(),
        );
        let sections = parse_judgment(&note);
        assert_eq!(
            sections.iter().map(|section| section.label.as_str()).collect::<Vec<_>>(),
            JUDGMENT_HEADINGS.to_vec(),
            "a compliant note parses whole",
        );
        assert!(
            sections
                .iter()
                .flat_map(|section| &section.lines)
                .all(|line| !line.contains("in the transcript named above")),
            "and the cap does not cut it",
        );
    }

    /// Both memory roots are excluded: the project's and the environment's
    /// own, where the cross-project notes live.
    #[test]
    fn the_memory_roots_cover_the_project_and_the_environment() {
        let roots = memory_roots(Path::new("/tmp/cfg"), Path::new("/tmp/proj"));
        assert!(
            roots.contains(&PathBuf::from("/tmp/cfg/projects/-tmp-proj/memory")),
            "the project's memory dir exactly, not its key dir: {roots:?}",
        );
        assert!(
            roots.contains(&PathBuf::from("/tmp/cfg/memory")),
            "and the environment's, where the cross-project notes live: {roots:?}",
        );
    }

    /// **Every spelling of the memory directory matches, and the boundary
    /// still holds.** A `~/` path, a canonical spelling of a symlinked root,
    /// and a sibling sharing the prefix are the three ways a raw string
    /// comparison gets this wrong.
    #[test]
    fn the_memory_path_match_covers_spellings_and_stops_at_the_boundary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("memory");
        std::fs::create_dir_all(&root).expect("dir");
        let roots = [root.clone()];
        assert!(
            is_memory_path(&root.join("note.md").to_string_lossy(), &roots),
            "the plain spelling",
        );
        let canonical = std::fs::canonicalize(&root).expect("canonical");
        assert!(
            is_memory_path(&canonical.join("note.md").to_string_lossy(), &roots),
            "a canonical spelling of a root reached through a symlink",
        );
        let home = dirs::home_dir().expect("home");
        assert!(
            is_memory_path("~/.claude-memory-test/note.md", &[home.join(".claude-memory-test")]),
            "a home-relative spelling",
        );
        assert!(
            !is_memory_path(&dir.path().join("memory-not/x.md").to_string_lossy(), &roots),
            "a sibling that merely shares the prefix is not inside",
        );
    }

    /// **The memory the handoff writes is not the session's work.** Counting
    /// it would displace the project files the successor needs.
    #[test]
    fn the_memory_the_handoff_writes_is_not_a_project_change() {
        let roots =
            [PathBuf::from("/tmp/cfg/projects/-tmp-proj/memory"), PathBuf::from("/tmp/cfg/memory")];
        let messages = vec![
            call(
                "m1",
                "Edit",
                serde_json::json!({ "file_path": "/tmp/cfg/projects/-tmp-proj/memory/note.md" }),
            ),
            call("m2", "Edit", serde_json::json!({ "file_path": "src/handoff.rs" })),
            call(
                "m3",
                "Read",
                serde_json::json!({ "file_path": "/tmp/cfg/projects/-tmp-proj/memory/index.md" }),
            ),
            call("m4", "Read", serde_json::json!({ "file_path": "src/lib.rs" })),
            call(
                "m5",
                "Edit",
                serde_json::json!({ "file_path": "/tmp/cfg/memory/til/a-lesson.md" }),
            ),
        ];
        let section = files_section(&messages, &HandoffGitState::default(), &roots);
        assert!(
            section.lines.contains(&"changed: src/handoff.rs".to_owned()),
            "only the project edit is a change: {:?}",
            section.lines,
        );
        assert!(
            section.lines.contains(&"read: src/lib.rs".to_owned()),
            "and only the project read is a read: {:?}",
            section.lines,
        );
    }

    /// The successor is measured against what its PREDECESSOR read; the
    /// predecessor's own handoff writes are not that.
    #[test]
    fn the_memory_the_handoff_reads_is_not_a_predecessor_read() {
        let roots =
            [PathBuf::from("/tmp/cfg/projects/-tmp-proj/memory"), PathBuf::from("/tmp/cfg/memory")];
        let messages = vec![
            call(
                "r1",
                "Read",
                serde_json::json!({ "file_path": "/tmp/cfg/projects/-tmp-proj/memory/index.md" }),
            ),
            call("r2", "Read", serde_json::json!({ "file_path": "src/lib.rs" })),
            call(
                "r3",
                "Read",
                serde_json::json!({ "file_path": "/tmp/cfg/memory/til/a-lesson.md" }),
            ),
        ];
        assert_eq!(read_set(&messages, &roots), vec!["src/lib.rs".to_owned()]);
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
