/**
 * A turn's messages, folded into the units a view draws.
 *
 * **This is the fold, and it is the client's.** It was `forge-server`'s until
 * the grouping moved here, and it is a port of `transcript.rs` rather than a
 * second design: the rules it folds by - the row a call is summarised under,
 * the status a run reports, which calls break a run - are that file's rules,
 * moved. A client that restated them would draw the same turn two ways the
 * first time either changed.
 *
 * Eight places it deliberately differs from the terminal, and each is a
 * decision rather than an accident:
 *
 * - a mutation folds as an `edit` family inside the run instead of breaking
 *   it;
 * - peer traffic and calls share one list: a message does not close the calls
 *   above it, and no row ever moves - every row keeps its arrival place;
 * - a thinking block draws as a row of that list, where the terminal has no
 *   row for one at all - its arm only sets a running status and counts
 *   characters;
 * - a question the assistant asked is a card rather than a call;
 * - an envelope that is not agent traffic is a notice rather than the reader's
 *   own turn, and the three external kinds that carry something to read - a
 *   cron fire, a Slack message, a Gotify push - join the list as rows of
 *   their own kind, the same shape every other row draws;
 * - a monitor is not in the conversation at all, because the inspector is its
 *   surface;
 * - a compaction boundary is a row at the cut, where the terminal draws none:
 *   the count is the terminal's marker, the cut is unmarked there, and this
 *   fold's `push` ends a run of calls at it;
 * - a hook's own lifecycle draws as one row per run, where the terminal's arm
 *   for those three frames is a no-op. Its summary chip - the one other hook
 *   surface it has - is matched here rather than dropped, and no capture holds
 *   that chip and a run's own frames together.
 *
 * **And a dispatched agent's frames are not the conversation either.** A
 * sub-agent's prose and calls belong to the SUBAGENTS surface, and drawn here
 * they duplicate that surface inside every turn that used one.
 */

import { cronNames } from './cron-names.svelte';
import { taskStatus, type CallStatus } from './families';
import { blocksOf, bodyOf, leafOf, type Block, type TaskFact, type ToolLeaf } from './leaves';
import { formatRateLimitSummary, rateLimitNoticeKey } from './rate-limit';
import { firstLine, isSlackId, stripEscapes } from './text';

/** One question the assistant asked, with what was answered. */
export interface AnsweredQuestion {
  question: string;
  picked_labels: string[];
  typed_note: string | null;
}

/** One call, as a row of the turn's work. */
export interface CallLeaf {
  /**
   * The fold's own name for the row.
   *
   * **The wire's `tool_use` id, or the frame and block it arrived in where the
   * wire gave none.** The fold computes it at the call (the way it names every
   * row), because a view that fell back to the id alone would key two id-less
   * calls of one family to an empty string - a duplicate key, which stops the
   * whole turn drawing at mount.
   */
  key: string;
  leaf: ToolLeaf;
}

/** How loudly a notice reads. */
export type NoticeSeverity = 'info' | 'warning' | 'error';

/**
 * A line the conversation carries that nobody typed.
 *
 * The envelope's own kind (`gotify`, `cron`, `slack`, `peer`, `worker`) is not
 * a field here: nothing draws it, and the text already names where the line
 * came from - an app, a workspace, an agent - so carrying it would be data
 * waiting for a renderer rather than data a view reads.
 */
export interface Notice {
  severity: NoticeSeverity;
  text: string;
  /**
   * A right-floated progress tag, which only the retry line carries: which
   * attempt of how many the CLI is on.
   */
  chip?: string;
  /** A quieter tail under the line, which the retry line's delay rides in. */
  sub?: string;
}

/**
 * Which row a peer card draws as: a message's direction, a failure, or the
 * verb whose answer the row holds.
 *
 * **Direction is carried by the words, and the mark reinforces them.** The
 * rows share one list, so the row's own word is what says which way a message
 * went; a verb's card says what it is by its title rather than by a direction
 * at all.
 */
export type PeerRow = 'sent' | 'arrived' | 'failed' | 'whoami' | 'list';

/**
 * The reader's own SEAT, which is the slot the page is drawing: the label is
 * part of it, because a worker and its own lead are two seats of one project.
 *
 * The org is what a counterparty's tag is compared against; the whole slot is
 * what tells a row whether it is the seat being read.
 */
export interface Self {
  org: string;
  project: string;
  label: string;
}

/** Where this session sits, as `agents__whoami` answers it. */
export interface SeatFacts {
  org: string;
  project: string;
  label: string;
  path: string;
  status: string;
}

/** One seat a `list` row opens onto. */
export interface SeatRow {
  /** The org the seat's project belongs to, which is part of its address. */
  org: string;
  label: string;
  project: string;
  /** The one clause that says what the seat is for. */
  what: string;
  /** The worker's own activity; empty for a project's own agent, which has none. */
  liveness: string;
}

/** One peer message or verb card, as the envelope it arrived in or the call that produced it. */
export interface PeerCard {
  /**
   * The wire's own id: the envelope's `m-…`, or the `tool_use` id of the call
   * that produced the card. It is what a view names the group by, so a handle
   * never has to be derived from a name two messages can share.
   */
  id: string;
  /** The row it draws as. */
  row: PeerRow;
  /**
   * Who the row is about: the counterparty's seat for a message, and the
   * verb's own word (`whoami`, `list`) for a card that answers one.
   */
  peer: string;
  /** The message itself, or a failure's reason; empty for the verb cards. */
  body: string;
  /** The counterparty's org, shown only when it is not the reader's own. */
  org: string | null;
  /**
   * What became of the call: a send with no result yet is still out, one whose
   * result came back in error did not arrive, and an envelope that arrived is
   * done.
   */
  status: CallStatus;
  /**
   * A send's own answer, which the row opens onto: the id the server returned
   * and the seat it went to.
   */
  ack: string | null;
  /** What a `whoami` row opens onto, when its answer has arrived and parsed. */
  seat: SeatFacts | null;
  /** What a `list` row opens onto, one row per reachable seat. */
  seats: SeatRow[];
}

/** One thing the model thought, as a row of the work. */
export interface ThoughtLeaf {
  /** The frame and block it came from, which is what the row is keyed by. */
  key: string;
  text: string;
}

/** One hook run, as a row of the work. */
export interface HookLeaf {
  /** The run's own id, or the frame it arrived on where the wire gave none. */
  key: string;
  run: HookRun;
}

/**
 * One row of a turn's work, in the order it arrived.
 *
 * Every row a stretch draws - a call, a peer card, a thought, a hook run, a
 * delivery - is one of these, and the list never reorders: a row that arrived
 * stays where it landed, so the fold's item sequence IS the drawn order.
 */
export type WorkRow =
  | ({ tag: 'call' } & CallLeaf)
  | { tag: 'card'; card: PeerCard }
  | ({ tag: 'thought' } & ThoughtLeaf)
  | ({ tag: 'hook' } & HookLeaf)
  | ({ tag: 'inbound' } & InboundLeaf);

/** What one turn's hooks did. */
export interface HookInfo {
  command: string;
  durationMs?: number;
}

/**
 * One hook's own run, as the frames that reported it fold together.
 *
 * The three subtypes are one run rather than three events: a start names the
 * hook, a progress carries what it has printed so far, and a response settles
 * it with the outcome and the code it exited on.
 *
 * `body` is held apart from the head because a hook's output is long and
 * secondary - a session-start hook prints the boilerplate it injects into the
 * session - so the row collapses it rather than drawing it.
 */
export interface HookRun {
  /** The hook the CLI matched, e.g. `SessionStart:startup`. */
  name: string;
  /** The event that fired it, and only where the name does not already say it. */
  event: string | null;
  /** Whether it failed, which is the code the response reports it exited on. */
  failed: boolean;
  /**
   * What the run printed, whole rather than clipped, and null where it printed
   * nothing - as whole as `hookRun` can make it, which is not as much as it
   * sounds: see the inference that doc records.
   */
  body: string | null;
}

/**
 * What a settled turn did: its clock, its API time, and the tokens and cost
 * the CLI reported. Every field is what the frame carried, and an absent one
 * draws as a dash rather than as a zero.
 */
export interface TurnInfo {
  /**
   * Whether the turn is still running: what the row's mark says and why its
   * settle-only figures draw absent. A record folded from the frames while
   * the turn runs carries it; one built from the Result frame never does.
   */
  running: boolean;
  /**
   * Whether the turn failed, which is what its own row leads with.
   *
   * The mark follows the turn: an interrupted turn draws the failure mark, and
   * the failure line under it carries the words. Two signals disagreeing on one
   * row - a check above "Turn failed" - is the defect this row was filed about.
   */
  failed: boolean;
  duration_ms: number | null;
  api_ms: number | null;
  ended_at_utc: string | null;
  model: string | null;
  thinking_tokens: number | null;
  input_tokens: number | null;
  output_tokens: number | null;
  cache_read_tokens: number | null;
  cache_written_tokens: number | null;
  session_cost_usd: number | null;
}

/**
 * One thing the reader attached to a turn, as the wire describes it.
 *
 * The wire carries no name: a pasted image is a mime type and a base64
 * payload, and a document the same. So the row draws the mime it does have,
 * with the size the payload's own length states.
 */
export interface AttachedFile {
  /** The block's own kind, which the row falls back to when there is no mime. */
  kind: string;
  mime: string | null;
  bytes: number | null;
}

/**
 * One thing a view draws, in the order the conversation produced it.
 *
 * `key` is the unit's identity in that list, and it exists because a live turn
 * is re-folded whole on every frame: a unit can land ABOVE others already
 * drawn, and a list keyed by position remounts everything below it, closing
 * whatever the reader had open. Keyed by this, the row is moved.
 */
export type Unit =
  /**
   * A turn the user wrote, with whatever they attached to it.
   *
   * `note` is what a drained prompt carries, written where the client released
   * it: the wait it spent in the pile and the word "sent". Client-side only -
   * the wire has no such field, and the row loses it when a page's own copy of
   * the message replaces it.
   */
  | { kind: 'user'; key: string; text: string; files: AttachedFile[]; note?: string }
  /** Prose the assistant wrote. */
  | { kind: 'text'; key: string; text: string }
  /**
   * A stretch of work: every call, message, thought, hook run and delivery on
   * its own row, in the order it arrived.
   *
   * **The rows never reorder.** An item lands where it arrived and stays
   * there, so the list reads as the turn happened and the order derives from
   * the item sequence - a page re-read from the transcript draws what the
   * live one drew.
   */
  | { kind: 'leaves'; key: string; rows: WorkRow[] }
  | { kind: 'question'; key: string; asked: AnsweredQuestion[] }
  | { kind: 'notice'; key: string; notice: Notice }
  /**
   * A skill the CLI loaded into the conversation.
   *
   * Its body arrives as the reader's own user frame, which nobody typed; the
   * row names the skill and holds the whole body, so the skill stays readable
   * without wearing an attribution it never had.
   */
  | { kind: 'skill'; key: string; name: string; body: string }
  | { kind: 'hooks'; key: string; actions: number; infos: HookInfo[]; errors: string[] }
  /**
   * Where the conversation was cut and the transcript replaced.
   *
   * The wire carries it as a `system` frame of its own subtype, and the fold
   * had no arm for it - so the frame that records the boundary drew nothing,
   * which is a frame dropped rather than a shape chosen. Its metadata carries
   * the trigger and the counts either side of the cut; `summary` is the
   * continuation prompt the CLI sends after it, which is what the row opens
   * onto, and it is null where that prompt never arrived.
   */
  | {
      kind: 'compaction';
      key: string;
      trigger: string | null;
      preTokens: number | null;
      postTokens: number | null;
      summary: string | null;
    }
  /** What a settled turn did, under the work it did it with. */
  | { kind: 'report'; key: string; info: TurnInfo };

/**
 * The tool a lifecycle block is drawn for, which the chat does not draw at all.
 *
 * Matched the way the terminal matches it, which is case-insensitively: a
 * lowercase `monitor` is the same call, and drawn as a tool row it duplicates
 * the inspector inside the turn it sits in.
 */
function isMonitor(name: string): boolean {
  return name.toLowerCase() === 'monitor';
}

/** Whether a call is a question the assistant asked. */
function isQuestion(name: string): boolean {
  return name.toLowerCase() === 'askuserquestion';
}

/** A string field of a JSON value. */
function str(value: unknown, key: string): string | null {
  const held = (value as Record<string, unknown> | null)?.[key];
  return typeof held === 'string' ? held : null;
}

/** One block's own JSON, when it is an object. */
function obj(value: unknown): Record<string, unknown> {
  return (value ?? {}) as Record<string, unknown>;
}

/**
 * The frame a page carries, as the fields this fold reads.
 *
 * Every read is tolerant on purpose: a frame is the CLI's own message and a
 * fold that threw on a shape it did not expect would take the whole
 * conversation with it.
 */
interface Frame {
  type?: unknown;
  subtype?: unknown;
  uuid?: unknown;
  compact_metadata?: unknown;
  parent_tool_use_id?: unknown;
  hookCount?: unknown;
  hookInfos?: unknown;
  hookErrors?: unknown;
  /** The run three frames report on: a start, its progress, and its response. */
  hook_id?: unknown;
  hook_name?: unknown;
  hook_event?: unknown;
  outcome?: unknown;
  exit_code?: unknown;
  /** `NoticeSeverity`, on the core's own line about a command. */
  severity?: unknown;
  output?: unknown;
  stdout?: unknown;
  stderr?: unknown;
  task_id?: unknown;
  tool_use_id?: unknown;
  is_backgrounded?: unknown;
  patch?: unknown;
  summary?: unknown;
  status?: unknown;
  estimated_tokens_delta?: unknown;
  /** The CLI's retry report: which attempt, of how many, after what, waiting how long. */
  attempt?: unknown;
  max_retries?: unknown;
  retry_delay_ms?: unknown;
  error_status?: unknown;
  /** The snapshot a `rate_limit_event` carries: status, window type, reset, overage. */
  rate_limit_info?: unknown;
  duration_ms?: unknown;
  duration_api_ms?: unknown;
  total_cost_usd?: unknown;
  is_error?: unknown;
  errors?: unknown;
  terminal_reason?: unknown;
  /** The string `Message::Error` carries, which is the CLI's own words for it. */
  error?: unknown;
  usage?: unknown;
  tool_use_result?: unknown;
  /** The CLI's own mark that a user frame is the harness talking, not the reader (#1543). */
  isSynthetic?: unknown;
  state?: unknown;
  timestamp?: unknown;
  message?: {
    id?: unknown;
    content?: unknown;
    model?: unknown;
    stop_reason?: unknown;
    usage?: unknown;
  };
}

/** Whether a frame came from a dispatched agent rather than the session's own. */
function isDispatched(frame: Frame): boolean {
  // The wire spells "no dispatch" as null and a dispatch's own tool-use id
  // otherwise, which is the same field and the same guard the server's fold
  // reads. The sidechain flag a transcript FILE carries is not on the wire at
  // all: `Message` has no such field, and no captured baseline contains the
  // string - so a guard keyed on it would read as working and suppress
  // nothing, which is worse than no guard.
  return typeof frame.parent_tool_use_id === 'string' && frame.parent_tool_use_id.trim() !== '';
}

/** What follows `marker`, or `null` when the text does not carry it. */
function after(text: string, marker: string): string | null {
  const at = text.indexOf(marker);
  return at === -1 ? null : text.slice(at + marker.length);
}

/**
 * What a base64 payload stands for, which is what an attachment weighed.
 *
 * Four characters carry three bytes, and the padding is not a byte: a row that
 * printed the encoded length would overstate every image it drew.
 */
function payloadBytes(data: unknown): number | null {
  if (typeof data !== 'string' || data === '') return null;
  const padding = data.endsWith('==') ? 2 : data.endsWith('=') ? 1 : 0;
  return Math.floor((data.length * 3) / 4) - padding;
}

/** The attachments a frame's content carries, in the order it carries them. */
function attachmentsOf(content: readonly unknown[]): AttachedFile[] {
  const out: AttachedFile[] = [];
  for (const block of blocksOf(content)) {
    if (block.type !== 'image' && block.type !== 'document') continue;
    // Both kinds nest their mime and their bytes under `source`, which is the
    // shape a tool result's image is read in too.
    const source = obj(block.source);
    out.push({
      kind: block.type,
      mime: str(source, 'media_type'),
      bytes: payloadBytes(source['data']),
    });
  }
  return out;
}

/**
 * The note a drained row carries, written by the client when it released the
 * row - the wait it spent in the pile and the word "sent". No wire frame has
 * this field: it is the client's own, and it goes when a page's copy of the
 * message replaces the row.
 */
function noteOf(frame: Frame): string | undefined {
  const note = (frame as { forge_note?: unknown }).forge_note;
  return typeof note === 'string' && note !== '' ? note : undefined;
}

/**
 * The words a `queued_command` block carries: a plain string for a typed
 * prompt, or a content-block array for a multi-modal one, where only the text
 * blocks are what the reader typed and every other block draws as a `[type]`
 * placeholder so the row shows something rather than a blank.
 *
 * A port of `queued_command_text` in forge-server's transcript fold, which is
 * the same policy the terminal reads it through.
 */
function queuedText(prompt: unknown): string {
  if (typeof prompt === 'string') return prompt;
  if (!Array.isArray(prompt)) {
    const raw = JSON.stringify(prompt);
    return raw === undefined ? '[unrenderable]' : raw;
  }
  return prompt
    .map((block) => {
      const type = str(block, 'type');
      if (type === 'text') return str(block, 'text');
      return type === null ? null : `[${type}]`;
    })
    .filter((part): part is string => part !== null)
    .join('\n');
}

/**
 * Whether a user frame is the CLI telling the MODEL that a skill was already
 * loaded: `Skill /unslop was loaded earlier (see the invoked-skills reminder
 * above); this is a NEW invocation...`.
 *
 * Nobody typed it, so it is a line the conversation carries rather than a turn
 * the reader took. Matched on the CLI's own sentence: the frame is also marked
 * synthetic, but the reminder has its own decided treatment and the sentence is
 * what names which reminder this is.
 */
function isSkillReminder(text: string): boolean {
  return text.startsWith('Skill /') && text.includes('was loaded earlier');
}

/**
 * A skill's body, which the CLI injects as the reader's own user frame.
 *
 * The frame is one text block: a plumbing line naming the skill's directory,
 * then the skill's markdown, so the row is built from that line - its name read
 * off the path and the line itself dropped from the body.
 *
 * **Not every body carries the line** (#1543): the frame reaches the fold
 * marked `isSynthetic` either way, which is what the fold trusts when this
 * recognizer finds nothing.
 */
export function skillBody(text: string): { name: string; body: string } | null {
  const [lead, ...rest] = text.split('\n');
  if (lead === undefined || !lead.startsWith('Base directory for this skill:')) return null;
  const parts = lead.slice('Base directory for this skill:'.length).trim().split('/');
  // A plugin-cached skill's path ends in its version, so the name is the last
  // segment that is not one: `.../ui-ux-pro-max/2.13.0` is `ui-ux-pro-max`.
  const name = parts.reverse().find((part) => part !== '' && !/^\d/.test(part)) ?? 'skill';
  return { name, body: rest.join('\n').trim() };
}

/**
 * Whether a frame is one of the local-command family, which the chat draws
 * nothing for.
 *
 * The four heads are the CLI's own: the caveat announcing what follows, the
 * echo of a command, and a local command's output. They are the reader's
 * typing in the LAUNCH terminal arriving as plumbing, and the terminal's own
 * chat filters the same set - so this is a decided ignore rather than an
 * accidental drop. Ved's ruling, 2026-10-03, after the audit measured the
 * family across every transcript: caveat 31, command echo 75, output 30, and
 * nothing else in the family.
 */
function isLocalCommand(text: string): boolean {
  const held = text.trimStart();
  return (
    held.startsWith('<local-command-caveat>') ||
    held.startsWith('<local-command-stdout>') ||
    held.startsWith('<command-name>') ||
    held.startsWith('<command-message>')
  );
}

/**
 * The summary a task notification's envelope carries, or null for every other
 * text.
 *
 * The CLI delivers a background task's end as a user frame whose text is this
 * envelope, with no stamp and no reader behind it - so the summary draws as an
 * info line of its own and the XML never reaches the page (#1680, Ved's shape:
 * the summary line alone). A frame carrying no summary is not claimed here.
 */
function taskNotificationOf(text: string): string | null {
  const held = text.trim();
  if (!held.startsWith('<task-notification>')) return null;
  const open = held.indexOf('<summary>');
  const close = held.indexOf('</summary>', open + 1);
  if (open === -1 || close === -1) return null;
  const summary = held.slice(open + '<summary>'.length, close).trim();
  return summary === '' ? null : summary;
}

/**
 * Whether a frame is the CLI's nudge after a response with no visible output.
 *
 * The harness asks the MODEL to continue; nobody typed it and the terminal
 * draws the raw bracket, where the page draws its own line (#1858). Keyed on
 * the opening sentence alone: a reworded tail still recognizes, and a
 * reworded head falls through to the raw draw - the frame is never lost.
 */
function noOutputNudge(text: string): boolean {
  return text.trim().startsWith('[Your previous response had no visible output.');
}

/** The harness's own line about an image, or null for every other text. */
function imageNoteOf(text: string): string | null {
  const held = text.trim();
  return held.startsWith('[Image: original ') ? held : null;
}

/**
 * A skill's body carried as the skill's OWN markdown, for a body the CLI
 * injects when the Skill tool loads one.
 *
 * That carrier has no plumbing line: the frame IS the skill, opening on its
 * title heading (`# PR Review Loop` for `pr-review-loop`). The name is the
 * heading, so the pairing is by that - normalized, because the two spellings
 * differ in case and separator - and the whole text is the body.
 */
export function headingNameOf(text: string): string | null {
  const lead = text.trimStart().split('\n')[0] ?? '';
  const match = /^#{1,6}\s+(.+)$/.exec(lead.trim());
  return match?.[1] ?? null;
}

/** A skill name as its words, so `pr-review-loop` and `Pr Review Loop` agree. */
function normalizedSkill(name: string): string {
  return name.toLowerCase().replaceAll(/[-_:]/g, ' ').replaceAll(/\s+/g, ' ').trim();
}

/**
 * Whether a `Skill` call's own input names the skill a body names.
 *
 * The spellings differ two ways: a plugin skill is `ui-ux-pro-max:ui-ux-pro-max`
 * where the path ends `ui-ux-pro-max`, and a tool-invoked body's heading is
 * `Pr Review Loop` where the call says `pr-review-loop`.
 */
export function namesSkill(want: string, name: string): boolean {
  if (want === name || want.endsWith(`:${name}`) || want.startsWith(`${name}:`)) return true;
  const held = normalizedSkill(want);
  const wanted = normalizedSkill(name);
  return held === wanted || held.includes(wanted) || wanted.includes(held);
}

/**
 * The continuation prompt a compaction leaves behind, which nobody typed.
 *
 * It arrives as a user frame right after the boundary frame, so the fold hands
 * it to the compaction row rather than drawing it as the reader's own turn.
 */
function isContinuation(text: string): boolean {
  return text.startsWith('This session is being continued from a previous conversation');
}

/**
 * Whether a `queued_command` block is the harness's own background-completion
 * notice, which nobody typed and this page does not draw.
 *
 * Both signals are read, because the terminal reads the text's own prefix
 * where the mode is the field that says which of the three kinds arrived: a
 * page keyed on one of them drifts from the other the first time either moves.
 */
function isCompletion(block: Block, words: string): boolean {
  return (
    str(block, 'commandMode') === 'task-notification' ||
    words.trimStart().startsWith('<task-notification>')
  );
}

/**
 * What a frame says when it is a mid-turn prompt's carrier: the words the CLI
 * queued, or `null` when it carries no prompt - or carries the harness's own
 * completion notice, which nobody typed.
 *
 * Exported because the conversation reads the same prompt in two carriers: the
 * frame forge echoes while the turn runs, and the `queued_command` block the
 * scan hoists the transcript's row into, which is what a page carries.
 */
export function queuedWords(message: unknown): string | null {
  const content = (message as { message?: { content?: unknown } } | null)?.message?.content;
  for (const block of blocksOf(content)) {
    if (block.type !== 'queued_command') continue;
    const words = queuedText(block.prompt);
    if (!isCompletion(block, words)) return words;
  }
  return null;
}

/** The text between two markers, and what follows the second. */
function between(text: string, open: string, close: string): [string, string] | null {
  const from = after(text, open);
  if (from === null) return null;
  const to = from.indexOf(close);
  return to === -1 ? null : [from.slice(0, to), from.slice(to + close.length)];
}

/** `'name' (org 'X')` as its two parts, from the text just after `from agent `. */
function sender(rest: string): { from: string; org: string } | null {
  if (!rest.startsWith("'")) return null;
  const name = rest.indexOf("' (org '");
  if (name === -1) return null;
  const org = rest.indexOf("')", name + 8);
  if (org === -1) return null;
  return { from: rest.slice(1, name), org: rest.slice(name + 8, org) };
}

/**
 * One peer message, with the one fact its row compares against the reader.
 *
 * `self` is the seat the page draws, and `null` when the fold runs without one
 * (the turn-opening probe): with no reader to compare against, the
 * counterparty carries no org tag.
 */
function peerCard(one: {
  id: string;
  row: PeerRow;
  peer: string;
  body: string;
  org: string | null;
  self: Self | null;
  status: CallStatus;
  ack?: string | null;
  seat?: SeatFacts | null;
  seats?: SeatRow[];
}): PeerCard {
  return {
    id: one.id,
    row: one.row,
    peer: one.peer,
    body: one.body,
    org: one.org !== null && (one.self === null || one.org !== one.self.org) ? one.org : null,
    status: one.status,
    ack: one.ack ?? null,
    seat: one.seat ?? null,
    seats: one.seats ?? [],
  };
}

/** What a send's own result says became of it, and a send with none is still out. */
function messageStatus(result: Block | undefined): CallStatus {
  if (result === undefined) return 'in_progress';
  return result.is_error === true ? 'failed' : 'completed';
}

/** The external kinds an inbound line draws as a row of its own. */
export type InboundKind = 'cron' | 'slack' | 'gotify';

/** One inbound delivery, as its row draws it. */
export interface InboundLeaf {
  /** The frame and block it arrived in, which is what the row is keyed by. */
  key: string;
  /**
   * The delivery's own kind, which the row reads for one thing: a cron fire's
   * title is its prompt's first line, so it renders as prose where the other
   * kinds' titles are names.
   */
  kind: InboundKind;
  /** The row's title: the schedule, the channel, or the app. */
  title: string;
  /** The whole of what arrived, which the row opens onto. */
  body: string;
  /** Whether the row reads at the warning tone: an elevated delivery. */
  elevated: boolean;
}

/**
 * What an envelope's prose turned into: a peer message, an inbound row, or a
 * line nobody typed.
 */
type Envelope =
  | { kind: 'peer'; card: PeerCard }
  | { kind: 'inbound'; inbound: InboundKind; title: string; body: string; elevated: boolean }
  | { kind: 'notice'; notice: Notice };

/**
 * The envelope text a user frame carries, when it is one this page knows.
 *
 * **A port of `forge_server::envelope::detect_inbound`, and it matches the
 * PRODUCER's strings rather than anything a renderer printed.** The producers
 * are `peers/types.rs`'s `to_prose`, `spawn.rs`'s Slack bundle and
 * `delivery.rs`'s cron wrapper; a matcher written from the terminal's
 * rendered output fires on nothing, and the failure is silent - the envelope
 * draws as the reader's own turn, which is a thing that looks like content
 * rather than like a bug.
 *
 * **Three of the arms below are the shapes a transcript recorded, not shapes
 * anything emits now**: the `Question`/`Reply` headers and the `Ask … failed
 * to deliver` notice belong to a session recorded before the peer surface
 * collapsed to one verb, and reopening it replays that history through this
 * same fold. They resolve to the rows the one verb produces.
 *
 * The header is everything between `[` and the first `]`, which matters for
 * the recorded kinds that carry a trailer: a question ends `- reply with ...`
 * and a reply ends `to your earlier ask`, both inside the brackets.
 */
export function inbound(text: string, self: Self | null): Envelope | null {
  if (!text.startsWith('[')) return null;
  const close = text.indexOf(']');
  if (close === -1) return null;
  const header = text.slice(1, close);
  const tail = text.slice(close + 1);
  // The peer wrappers land their body tail `]\n\n`; the notice kinds have
  // their own spacing and are read below.
  const body = tail.startsWith('\n\n') ? tail.slice(2) : '';

  const message = header.startsWith('Message id=');
  const recordedQuestion = header.startsWith('Question id=');
  const recordedReply = header.startsWith('Reply id=');
  if (message || recordedQuestion || recordedReply) {
    const spec = header.slice(header.indexOf('=') + 1);
    const at = spec.indexOf(' from agent ');
    if (at === -1) return null;
    const who = sender(spec.slice(at + ' from agent '.length));
    if (who === null) return null;
    return {
      kind: 'peer',
      card: peerCard({
        id: spec.slice(0, at),
        row: 'arrived',
        peer: who.from,
        body,
        org: who.org,
        self,
        status: 'completed',
      }),
    };
  }

  // The target of a failure, from either the current header or a recorded one.
  const failed = header.startsWith('Message to agent ')
    ? header.slice('Message to agent '.length)
    : header.startsWith('Ask id=')
      ? after(header.slice('Ask id='.length), ' to agent ')
      : null;
  if (failed !== null && header.includes('failed to deliver:')) {
    const who = sender(failed);
    if (who === null) return null;
    const reason = (after(failed, 'failed to deliver:') ?? '').trim();
    return {
      kind: 'peer',
      card: peerCard({
        // No id of its own. Two failures from one seat in a turn are ordinary -
        // a bucket of parked messages is acked one notice per message, and a
        // resumed transcript replays them - so the fold keys this card by the
        // frame and block it arrived in rather than by a name two cards share.
        id: '',
        row: 'failed',
        peer: who.from,
        body: reason,
        org: who.org,
        self,
        status: 'failed',
      }),
    };
  }

  if (header.startsWith("Worker '")) {
    const opened = after(header.slice("Worker '".length), '');
    const label = opened === null ? null : between(opened, '', "' spawn failed id=");
    if (label === null) return null;
    const reason = after(label[1], ': ') ?? '';
    return {
      kind: 'notice',
      notice: {
        severity: 'warning',

        text: `'${label[0]}' failed to spawn: ${reason}`.trimEnd(),
      },
    };
  }

  if (header.startsWith("Gotify - app '")) {
    const parts = between(header.slice("Gotify - app '".length), '', "', priority ");
    if (parts === null) return null;
    const priority = Number(parts[1]);
    if (!Number.isInteger(priority)) return null;
    // A Gotify body sits one newline tail `]`: a title line, then the text.
    const raw = tail.startsWith('\n') ? tail.slice(1) : tail;
    const cut = raw.indexOf('\n');
    const title = cut === -1 ? raw : raw.slice(0, cut);
    const message = cut === -1 ? '' : raw.slice(cut + 1);
    return {
      kind: 'inbound',
      inbound: 'gotify',
      title: `${parts[0]} \u{b7} priority ${priority}`,
      body: `${title}\n${message}`.trimEnd(),
      elevated: priority >= 5,
    };
  }

  if (header.startsWith("Slack - workspace '")) {
    const parts = between(header.slice("Slack - workspace '".length), '', "', ");
    if (parts === null) return null;
    const cut = tail.indexOf('\n');
    if (cut === -1) return null;
    const rest = tail.slice(cut + 1);
    const split = rest.indexOf(': ');
    if (split === -1) return null;
    const author = rest.slice(0, split);
    const said = rest.slice(split + 2);
    // A bundle's members each name their own author, so the header names none
    // of them - and an id or the producer's `unknown` is not a name.
    const bundle = /\(\d+ messages\)/.test(tail.slice(0, cut));
    const named = bundle || author === 'unknown' || isSlackId(author) ? null : author;
    const head =
      named === null
        ? `${parts[0]} \u{b7} ${parts[1]}`
        : `${parts[0]} \u{b7} ${parts[1]} \u{b7} ${named}`;
    return {
      kind: 'inbound',
      inbound: 'slack',
      title: head,
      body: said.trimEnd(),
      elevated: false,
    };
  }

  if (header === 'Cron') {
    return { kind: 'inbound', inbound: 'cron', title: firstLine(body), body, elevated: false };
  }

  return null;
}

/** How one message-sending tool's input names its target and its body. */
interface MessageTool {
  /** The input key carrying the message. */
  body: string;
  /** Where the target sits: a slot's address, or a bare name. */
  target: 'address' | 'name' | 'label';
}

/**
 * Every tool a message card draws for.
 *
 * `agents__send_message` is the live one; the six below it are what a
 * transcript recorded before the verbs were folded holds, and reopening one
 * draws those rows again. Each recorded name kept its own input shape, so the
 * table carries the shape rather than assuming the live one.
 */
const MESSAGE_TOOLS: Readonly<Record<string, MessageTool>> = {
  mcp__forge__agents__send_message: { body: 'message', target: 'address' },
  // replay-only: agents__ask
  mcp__forge__agents__ask: { body: 'prompt', target: 'address' },
  // replay-only: agents__tell
  mcp__forge__agents__tell: { body: 'message', target: 'address' },
  // replay-only: peers__ask_agent
  mcp__forge__peers__ask_agent: { body: 'prompt', target: 'name' },
  // replay-only: peers__tell_agent
  mcp__forge__peers__tell_agent: { body: 'message', target: 'name' },
  // replay-only: workers__ask
  mcp__forge__workers__ask: { body: 'question', target: 'label' },
  // replay-only: workers__tell
  mcp__forge__workers__tell: { body: 'message', target: 'label' },
};

/** The JSON a tool result carries, when its text is JSON at all. */
function resultJson(result: Block | undefined): unknown {
  if (result === undefined) return null;
  for (const part of bodyOf(result.content)) {
    if (part.kind !== 'text') continue;
    try {
      return JSON.parse(part.text) as unknown;
    } catch {
      return null;
    }
  }
  return null;
}

/**
 * What a result said in prose, which is where a refusal puts its reason.
 *
 * A tool that answers with JSON has nothing here to read; one that refuses
 * answers with the CLI's own sentence - raw, or inside the CLI's envelope for
 * the MCP-result shape, which the fold reads off only where the call failed -
 * and that sentence is what a failure row has to show rather than the input it
 * was called with.
 */
function resultText(result: Block | undefined, failed: boolean): string | null {
  if (result === undefined) return null;
  for (const part of bodyOf(result.content, failed)) {
    const text =
      part.kind === 'error' ? part.message : part.kind === 'text' ? part.text.trim() : '';
    if (text !== '') return text;
  }
  return null;
}

/** The id and seat a send's own answer carries, as the row's ack line reads them. */
function sendAck(result: Block | undefined): string | null {
  const answer = obj(resultJson(result));
  const id = str(answer, 'id');
  const to = obj(answer['to']);
  const org = str(to, 'org');
  const project = str(to, 'project');
  const label = str(to, 'label');
  if (id === null || org === null || project === null || label === null) return null;
  return `id ${id} \u{b7} to ${org}/${project}/${label}`;
}

/** Where this session sits, from what `whoami` answered. */
function seatFacts(result: Block | undefined): SeatFacts | null {
  const answer = obj(resultJson(result));
  const slot = obj(answer['slot']);
  const org = str(slot, 'org');
  const project = str(slot, 'project');
  const label = str(slot, 'label');
  if (org === null || project === null || label === null) return null;
  return {
    org,
    project,
    label,
    path: str(answer, 'path') ?? '',
    status: str(answer, 'status') ?? '',
  };
}

/**
 * The seats a `list` answer carries, in the order the server returned them.
 *
 * **Two row shapes arrive in one array** - a project's own agent and a worker,
 * each with its own snapshot fields - and nothing in the answer labels which
 * is which, so the slot's own label does: `lead` is the project's agent, and
 * anything else is a worker and carries the charter it was spawned with. A row
 * whose slot will not read is dropped rather than drawn as a nameless row.
 *
 * **What a row says it is for is the reader's own address, read three ways.**
 * Only the seat the page is DRAWING names itself - the whole slot, so a worker
 * reading its own project's list does not call its siblings `this session`. A
 * project's own agent is the reader's if it is the seat being read, the
 * reader's own project's if only the project matches, and another project's
 * otherwise; every other row is a worker and carries the phrase of its charter.
 *
 * **What a row is for and its liveness both depend on the shape.** A
 * project's agent has no activity to report, so its liveness is empty; a
 * worker's comes from `activity`, which is the axis that keeps moving, not
 * from the spawn outcome `status` freezes at `Running`.
 */
function seatRows(result: Block | undefined, self: Self | null): SeatRow[] {
  const answer = resultJson(result);
  if (!Array.isArray(answer)) return [];
  const rows: SeatRow[] = [];
  for (const entry of answer) {
    const one = obj(entry);
    const slot = obj(one['slot']);
    const label = str(slot, 'label');
    const project = str(slot, 'project');
    if (label === null || project === null) continue;
    const org = str(slot, 'org') ?? '';
    // The reader's own address, read twice: the whole slot is the seat the
    // page is drawing, and the org and project alone are the one it sits in.
    const inMyProject = self !== null && org === self.org && project === self.project;
    const mySeat = inMyProject && self !== null && self.label === label;
    const agent = label === 'lead';
    rows.push({
      org,
      label,
      project,
      what: mySeat
        ? 'this session'
        : agent
          ? inMyProject
            ? "the project's own agent"
            : 'another project'
          : phrase(str(one, 'charter') ?? ''),
      liveness: agent ? '' : lower(str(one, 'activity') ?? str(one, 'status') ?? ''),
    });
  }
  return rows;
}

/**
 * A phrase-sized cut of `text`, for the one clause a seat row says it is for.
 *
 * The budget is the terminal's own collapsed-row one: a charter's first line
 * is a sentence, and a row that shows the whole of it stops reading as a row.
 */
function phrase(text: string): string {
  const line = firstLine(text);
  return line.length > 60 ? `${line.slice(0, 60).trimEnd()}\u{2026}` : line;
}

/**
 * The wire's own word for a state, as a row prints it.
 *
 * `SessionLifecycleState` and `WorkerLiveness` serialize their variants as
 * they are spelled in Rust - `Running`, `Idle` - where `PeerLiveness` is
 * snake_case, so one row's liveness read `Running` beside another's `running`.
 */
function lower(word: string): string {
  return word.charAt(0).toLowerCase() + word.slice(1);
}

/**
 * The peer card an outbound call draws, when it is one.
 *
 * **The card IS the row**: a call that draws one is not drawn as a tool row as
 * well, which is what keeps the rows one per event rather than two.
 *
 * Each name carries its own input and answer shape. A send names a target and
 * answers with the id it went out under; `whoami` answers with this seat's
 * facts and `list` with the seats it can reach, and neither names a target at
 * all. A message call with no readable target is not a card - every send names
 * one - so it draws as the tool row it would have been.
 */
function outbound(
  name: string,
  input: unknown,
  self: Self | null,
  result: Block | undefined,
  id: string,
): PeerCard | null {
  const fields = obj(input);
  const status = messageStatus(result);

  const send = MESSAGE_TOOLS[name];
  if (send !== undefined) {
    const peer =
      send.target === 'address'
        ? address(fields)
        : send.target === 'name'
          ? str(fields, 'target')
          : str(fields, 'label');
    if (peer === null) return null;
    const failed = status === 'failed';
    return peerCard({
      id,
      // A send whose own result came back in error never reached anybody, so
      // its row is the failure row rather than a second row beside it.
      row: failed ? 'failed' : 'sent',
      peer,
      // **A failure says what went wrong, not what was sent.** The row's own
      // words read `failed to deliver: <reason>`, and the reason is the
      // refusal the call came back with. A refusal with no words leaves the
      // tail off rather than putting the message under it, which would say the
      // words arrived.
      body: failed ? (resultText(result, failed) ?? '') : (str(fields, send.body) ?? ''),
      org: null,
      self,
      status,
      ack: sendAck(result),
    });
  }

  if (name === 'mcp__forge__agents__whoami') {
    return peerCard({
      id,
      row: 'whoami',
      peer: 'whoami',
      body: '',
      org: null,
      self,
      status,
      seat: seatFacts(result),
    });
  }
  if (name === 'mcp__forge__agents__list') {
    return peerCard({
      id,
      row: 'list',
      peer: 'list',
      body: '',
      org: null,
      self,
      status,
      seats: seatRows(result, self),
    });
  }
  return null;
}

/** The `${project}` or `${project}/${label}` a header shows for a call's target. */
function address(fields: Record<string, unknown>): string | null {
  const project = str(fields, 'project');
  if (project === null) return null;
  const label = str(fields, 'label');
  return label === null || label === 'lead' ? project : `${project}/${label}`;
}

/** The card a question call draws, with whatever the person answered. */
function questionCard(input: unknown, answer: unknown, key: string): Unit | null {
  const questions = obj(input)['questions'];
  const answers = obj(obj(answer)['answers']);
  const annotations = obj(obj(answer)['annotations']);
  const asked: AnsweredQuestion[] = [];

  for (const entry of Array.isArray(questions) ? questions : []) {
    const text = str(entry, 'question') ?? '';
    const options = obj(entry)['options'];
    const labels = (Array.isArray(options) ? options : [])
      .map((option) => str(option, 'label'))
      .filter((label): label is string => label !== null);

    const recorded = answers[text];
    const values = Array.isArray(recorded)
      ? recorded.filter((value): value is string => typeof value === 'string')
      : typeof recorded === 'string'
        ? [recorded]
        : [];
    const picked = values.filter((value) => value !== '' && labels.includes(value));
    // The annotation first: it is the field the CLI fills with what was TYPED,
    // and a selected value that is not one of the question's own labels is the
    // escape row's label - so reading values first drew "you typed: Tell the
    // agent something else" where the reader's own words should be.
    const typed =
      str(obj(annotations[text]), 'notes') ??
      values.find((value) => value !== '' && !labels.includes(value)) ??
      null;

    asked.push({ question: text, picked_labels: picked, typed_note: typed === '' ? null : typed });
  }
  // A question still waiting is not drawn here at all: the dock is its row
  // while it waits, and the card that carried both the question and its answer
  // read as a second copy of the prompt rather than as the record of one. It
  // appears the moment an answer lands.
  if (!asked.some((entry) => entry.picked_labels.length > 0 || entry.typed_note !== null)) {
    return null;
  }
  return { kind: 'question', key, asked };
}

/** One settled turn's report, from the frame that recorded it. */
function reportOf(
  frame: Frame,
  model: string | null,
  thinking: number | null,
  endedAt: string | null,
): TurnInfo {
  const usage = obj(frame.usage);
  const count = (key: string): number | null => {
    const value = usage[key];
    return typeof value === 'number' ? value : null;
  };
  return {
    running: false,
    failed: frame.is_error === true,
    duration_ms: typeof frame.duration_ms === 'number' ? frame.duration_ms : null,
    api_ms: typeof frame.duration_api_ms === 'number' ? frame.duration_api_ms : null,
    ended_at_utc: endedAt,
    model,
    thinking_tokens: thinking,
    input_tokens: count('input_tokens'),
    output_tokens: count('output_tokens'),
    cache_read_tokens: count('cache_read_input_tokens'),
    cache_written_tokens: count('cache_creation_input_tokens'),
    session_cost_usd: typeof frame.total_cost_usd === 'number' ? frame.total_cost_usd : null,
  };
}

/**
 * A word a frame carries beside the sentence that already means it.
 *
 * The frames below write their own ending into some of their sentences and
 * not others - the CLI's end reason is inside some subtypes and not others -
 * so the word is drawn exactly when the sentence does not already say it.
 */
function beside(sentence: string, word: string | null): string {
  if (word === null || word === '' || sentence.includes(word)) return sentence;
  return sentence === '' ? word : `${sentence} \u{b7} ${word}`;
}

/**
 * What a turn that failed says, from the frame that recorded the failure.
 *
 * The subtype and the reason the CLI ended the stream with are what the reader
 * is owed - `error_during_execution`, `aborted_streaming` - and the errors
 * array, when the frame carries one, is the CLI's own diagnostic of it. Both
 * go in one line, because a failure is one thing.
 */
/**
 * The core's own severity word, narrowed where it enters.
 *
 * The wire's `NoticeSeverity` is two levels, but a line this page authors
 * itself may carry the third the notice row draws (the rate-limit explainer
 * at warning); a word nobody classifies reads as informational, because a
 * line nobody can classify is not a failure to shout about.
 */
function noticeSeverity(value: unknown): NoticeSeverity {
  if (value === 'error') return 'error';
  if (value === 'warning') return 'warning';
  return 'info';
}

/**
 * The rank a notice key sits at, the terminal's own `NoticeStage` order
 * (`Warning < Rejected < PlanLimitTurnError`): a later stage may rewrite the
 * line already drawn, a lower one may not.
 */
const NOTICE_STAGE = { warning: 0, rejected: 1 } as const;

/**
 * The terminal's own words for a retry classification, so the two views name
 * the same failure the same way (`app/events/api_retry.rs`'s `error_label`).
 */
function retryLabel(value: unknown): string {
  switch (value) {
    case 'authentication_failed':
    case 'billing_error':
    case 'rate_limit':
    case 'invalid_request':
    case 'server_error':
    case 'max_output_tokens':
      return value;
    default:
      return 'connection error';
  }
}

/** The delay as the terminal writes it: tenths of a second past 1s, else ms. */
function retryDelay(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  const tenths = Math.floor((ms + 50) / 100);
  return `${Math.floor(tenths / 10)}.${tenths % 10}s`;
}

function turnFailure(frame: Frame): Notice | null {
  if (frame.is_error !== true) return null;
  const subtype = str(frame, 'subtype');
  const head = beside(subtype === 'success' ? '' : (subtype ?? ''), str(frame, 'terminal_reason'));
  const errors = (Array.isArray(frame.errors) ? frame.errors : []).filter(
    (one): one is string => typeof one === 'string' && one.trim() !== '',
  );
  const said = [head === '' ? 'Turn failed.' : `Turn failed: ${head}`, ...errors];
  return { severity: 'error', text: said.join('\n').trimEnd() };
}

/**
 * The line a BACKGROUNDED task leaves behind, from the frame that ended it.
 *
 * The task frames arrive for every task the CLI runs, a foreground call and a
 * dispatched agent among them, and for those the note would repeat what the
 * row already says - the result that came back, or the report that is the
 * call's own body. It is drawn where the wire says the task outlives its turn.
 *
 * The harness writes its own sentence for a command that finished and only the
 * task's description for one that was stopped, so the wire's status word is
 * drawn beside the summary exactly when the summary does not already say it.
 * The tone follows that word the same way - green only for a task that
 * finished, red only for one that failed or was killed, and no tone for a word
 * this page does not know, because an unknown word is not a failure.
 */
function taskLine(summary: string | null, wire: string | null): TaskFact['note'] {
  if (summary === null || summary.trim() === '') return null;
  const said = wire !== null && !summary.includes(wire) ? `${summary} \u{b7} ${wire}` : summary;
  const tone =
    wire === 'completed'
      ? 'sum'
      : wire === 'failed' || wire === 'killed' || wire === 'stopped'
        ? 'fail'
        : null;
  return { text: said, tone };
}

/** What the hook a frame reports has printed, as the frame spells it. */
function hookOutput(frame: Frame): string | null {
  // The two raw streams are read only where the combined output is empty: a
  // hook that wrote to one of them and not the other would otherwise draw as a
  // hook that said nothing.
  for (const key of ['output', 'stdout', 'stderr']) {
    const held = str(frame, key);
    if (held !== null && held !== '') return held;
  }
  return null;
}

/** Whether the hook failed, which is the code the response reports it exited on. */
function hookFailed(frame: Frame): boolean {
  const exit = frame.exit_code;
  return typeof exit === 'number' && exit !== 0;
}

/**
 * The run a hook's own lifecycle frame reports, as the row's parts.
 *
 * **That a response repeats its progress rather than extending it is inferred,
 * not measured.** The progress frames' output is cumulative across every
 * captured run and the field is documented as the combined output the session
 * saw, but the capture redactor stubs every hook body value-blind, so no
 * fixture can show the response half. The body is therefore drawn whole from
 * whichever frame the run sent last.
 *
 * The event rides beside the name only where the name does not already carry
 * it, the rule the frames' own sentences are read by (`beside`).
 */
function hookRun(frame: Frame): HookRun {
  const name = str(frame, 'hook_name') ?? '';
  const fired = str(frame, 'hook_event') ?? '';
  return {
    name,
    event: fired !== '' && !name.includes(fired) ? fired : null,
    failed: hookFailed(frame),
    body: hookOutput(frame),
  };
}

/**
 * What a transcript's own completion notice says, which is the same fields the
 * `task_notification` frame carries.
 *
 * A transcript holds no task frames at all - the CLI persists a background
 * task's ending as a `<task-notification>` text block - so a page read has
 * only this to end a call with, and without it the call reverts to drawing as
 * finished at launch on every reconnect.
 */
function noticeFields(words: string): {
  call: string | null;
  status: string | null;
  summary: string | null;
} {
  const inside = (tag: string): string | null => {
    const open = words.indexOf(`<${tag}>`);
    if (open === -1) return null;
    const close = words.indexOf(`</${tag}>`, open);
    return close === -1 ? null : words.slice(open + tag.length + 2, close).trim();
  };
  return { call: inside('tool-use-id'), status: inside('status'), summary: inside('summary') };
}

/**
 * Fold a turn's messages into the units a view draws.
 *
 * A `turn` is one page's worth of conversation as the server cut it, so this
 * never has to decide where a turn begins: it decides how the blocks inside
 * one read.
 *
 * `live` is the caller's own fact - the turn is still being written - and it
 * cannot be read off the frames: a page read from the transcript carries no
 * result frame either, because the transcript holds none, so "no result" also
 * means "read from disk". A turn still being written draws a running report
 * from what its frames already carry.
 *
 * `ended` is the caller's OTHER half of that fact, and the default says
 * "unknown": a turn the core reports as settled is a history the CLI has
 * finished writing, so a call the frames never answered is the restart's
 * unterminated call and settles failed. The terminal's own resume does the
 * same (`finalize_turn_runtime_artifacts(Failed)`), where leaving it pending
 * spins the row forever.
 */
export function fold(
  messages: readonly unknown[],
  self: Self | null = null,
  live = false,
  ended = false,
): Unit[] {
  const frames = messages as Frame[];
  /** Every result the turn holds, by the call it answers. */
  const results = new Map<string, ReturnType<typeof blocksOf>[number]>();
  /** The result's own record, by the call it answers: a question's answer, and a mutation's hunks and marks. */
  const records = new Map<string, unknown>();
  /**
   * The LAST frame the turn failed on, which is what bounds the sweep.
   *
   * A call is abandoned when any failing frame follows it, so the bound is the
   * last one rather than the first: a fold holding two failing turns settles
   * both, and first-wins leaves the second turn's calls pending - the run's
   * roll-up saying work is still going, which is the complaint the interrupt
   * issue is filed about.
   *
   * **And the sweep is bounded by that frame, not by the turn.** The live path
   * accumulates every frame since the last page into ONE turn, so a sweep that
   * took the whole turn would mark calls the CLI started AFTERWARDS as failed
   * while they are still running: interrupt a turn, prompt again, and the new
   * turn's first call draws red until its own result lands.
   */
  let failedAt: number | null = null;
  /**
   * The LAST `result` frame's index, which is where the turn ENDED.
   *
   * A call opened before it that never came back is an unterminated call -
   * the restart case: forge kills and respawns the CLI mid-turn, the replayed
   * history keeps the call's `tool_use` with no result, and the resumed turn
   * never answers it. The terminal settles exactly these as failed on its
   * resume (`finalize_turn_runtime_artifacts(Failed)`, "resumed a tool call
   * whose result never arrived"), where a fold reading only failing frames
   * leaves the row spinning forever.
   */
  let resultAt: number | null = null;
  /** What the wire reported about each backgrounded call, by call. */
  const tasks = new Map<string, TaskFact>();
  /** The call a task belongs to, which the frames that carry one name. */
  const owners = new Map<string, string>();
  /**
   * The `Skill` calls this turn holds, by the name each asked for and with the
   * row each drew, so a body arriving behind its call still finds the row that
   * should open onto it - and does so even across a flush, because the leaf is
   * the row the list holds rather than a copy.
   */
  const skillCalls: Array<{ want: string; leaf: ToolLeaf }> = [];
  for (const [at, frame] of frames.entries()) {
    // A dispatched agent's frames are not the conversation, and its verdict is
    // not the session's: a sub-agent's failed result says nothing about the
    // turn the parent is still running. The drawing loop skips these frames
    // the same way, so a pre-pass that read one would finalize calls the loop
    // never drew a row for.
    if (isDispatched(frame)) continue;
    // A fatal error is the CLI's last-gasp signal before teardown: no result
    // frame follows it, so it is the turn's own verdict just as a failed
    // result is.
    if (frame.type === 'error' || (frame.type === 'result' && frame.is_error === true)) {
      failedAt = at;
    }
    if (frame.type === 'result') {
      resultAt = at;
    }
    if (frame.type === 'system') {
      const task = str(frame, 'task_id');
      const call = str(frame, 'tool_use_id');
      if (frame.subtype === 'task_started' && call !== null) {
        if (task !== null) owners.set(task, call);
        // What the wire says about the task living past its turn, which is the
        // only case the harness's summary is worth a line: a foreground call's
        // result and a dispatch's report are already on the row.
        tasks.set(call, {
          status: 'in_progress',
          note: null,
          backgrounded: frame.is_backgrounded === true,
        });
      } else if (frame.subtype === 'task_updated' || frame.subtype === 'task_notification') {
        // `task_updated` names only the task, so an update whose own
        // `task_started` was never seen cannot be placed and is dropped rather
        // than guessed at - which is the call the terminal makes on the same
        // frame, and for the same reason: the wrong call would be worse than
        // none.
        const owner = call ?? (task === null ? null : (owners.get(task) ?? null));
        if (owner === null) continue;
        const held: TaskFact = tasks.get(owner) ?? {
          status: 'in_progress',
          note: null,
          backgrounded: false,
        };
        const wire = str(frame, 'status') ?? str(obj(frame.patch), 'status');
        tasks.set(owner, {
          status: taskStatus(wire) ?? held.status,
          note:
            frame.subtype === 'task_notification'
              ? taskLine(str(frame, 'summary'), wire)
              : held.note,
          backgrounded: held.backgrounded,
        });
      }
      continue;
    }
    if (frame.type !== 'user') continue;
    for (const block of blocksOf(frame.message?.content)) {
      // The transcript's own ending for a backgrounded call, which arrives as
      // a text block rather than as the frames the live wire sends: a page read
      // carries no task frames at all. It closes the endings persisted in THIS
      // carrier; the same ending also reaches a transcript as a plain user row
      // carrying the same XML, and what goes unread there is the ENDING - the
      // row itself draws, as the reader's own turn, raw XML and all (#1364).
      if (block.type === 'queued_command') {
        const words = queuedText(block.prompt);
        if (!isCompletion(block, words)) continue;
        const said = noticeFields(words);
        if (said.call === null) continue;
        const held = tasks.get(said.call);
        tasks.set(said.call, {
          // The word the notice carries, or the status the call already had:
          // a notice whose status is unreadable says the task ended without
          // saying how, which is not a reason to walk a finished call back to
          // running. (10 of this machine's 3,141 notices carry no status.)
          status: taskStatus(said.status) ?? held?.status ?? 'in_progress',
          note: taskLine(said.summary, said.status),
          backgrounded: true,
        });
        continue;
      }
      if (block.type === 'tool_result' && typeof block.tool_use_id === 'string') {
        results.set(block.tool_use_id, block);
        // The result's own record rides beside the block, keyed the same way:
        // a question is drawn from it rather than from the result's text -
        // which is the CLI saying it was answered - and a mutation's hunks and
        // its marks are in it too.
        if (frame.tool_use_result !== undefined) {
          records.set(block.tool_use_id, frame.tool_use_result);
        }
      }
    }
  }

  /** What names a unit for the list: the frame it came from, and the part in it. */
  const keyOf = (at: number, frame: Frame, part: string | number): string => {
    const id = typeof frame.uuid === 'string' ? frame.uuid : `f${at}`;
    return `${id}#${part}`;
  };

  const units: Unit[] = [];
  /**
   * The stretch's rows, before they flush: tool calls, peer cards, thoughts,
   * hook runs and deliveries, in the order they arrived.
   *
   * A peer message does not close the calls above it, and a call does not close
   * the traffic: the list is one sequence, and an item joins it where it lands.
   */
  let pending: WorkRow[] = [];
  let model: string | null = null;
  let thinking: number | null = null;
  /**
   * The instant the turn's own last row carried.
   *
   * A settled turn's row can only read a clock from what its frames wrote:
   * the result frame carries none, and a transcript-derived turn has no result
   * frame at all - so a page that never read one would draw the row's first
   * fact as a permanent dash.
   */
  let endedAt: string | null = null;
  /** The instant the turn's first frame carried, which is where its clock starts. */
  let startedAt: string | null = null;
  /** Whether a result frame has landed, which is what ends a live turn. */
  let sawResult = false;
  /**
   * The usage the turn's frames carry, keyed by the message each belongs to.
   *
   * The CLI draws one assistant message as several frames, one per content
   * block, and every one of them repeats the whole call's usage block - so a
   * repeat overwrites rather than adds. Summing the frames instead counts a
   * two-block message twice, which is the rule the terminal's `LiveTurn::record`
   * exists to apply.
   */
  const usageByMessage = new Map<
    string,
    { input: number; output: number; read: number; written: number }
  >();
  /**
   * The row the last boundary opened, by index, so the continuation prompt
   * that follows it lands on that row rather than drawing as the reader's.
   */
  let lastCompaction: number | null = null;
  /** The stage each notice key sits at, so a walk-back cannot soften a line. */
  const noticeStages = new Map<string, number>();

  /**
   * Hang a skill's body on the call that loaded it.
   *
   * The first unclaimed call takes it - the order bodies arrive in is the order
   * their calls were made - and the row is the leaf itself, so a run that
   * flushed between the two changes nothing. `name` is the body's own evidence
   * where it carries one; `null` is a body whose text names no skill, claimed
   * by position the way the synthetic mark says it should be (#1543).
   */
  const attachSkillBody = (name: string | null, body: string): boolean => {
    for (const held of skillCalls) {
      if (held.leaf.skill !== null) continue;
      if (name !== null && !namesSkill(held.want, name)) continue;
      held.leaf.skill = body;
      return true;
    }
    return false;
  };

  /**
   * Rewrite the row a hook run opened, wherever it currently sits.
   *
   * A later frame of one run replaces that run's row rather than drawing
   * beside it, in place: the list never reorders, so an update never moves a
   * row the reader is looking at.
   */
  const rewriteHook = (key: string, run: HookRun): boolean => {
    for (const [index, item] of pending.entries()) {
      if (item.tag !== 'hook' || item.key !== key) continue;
      pending[index] = { tag: 'hook', key, run };
      return true;
    }
    for (let at = units.length - 1; at >= 0; at -= 1) {
      const unit = units[at];
      if (unit?.kind !== 'leaves') continue;
      const held = unit.rows.find((row) => row.tag === 'hook' && row.key === key);
      if (held === undefined) continue;
      unit.rows = unit.rows.map((row) =>
        row.tag === 'hook' && row.key === key ? { tag: 'hook', key, run } : row,
      );
      return true;
    }
    return false;
  };

  /**
   * Rewrite the line this key already drew, or open it.
   *
   * A run reports every step it takes and the row is the RUN, so a later frame
   * replaces its own line - the shape the terminal's deduped turn notice
   * draws, and why a storm is one row rather than fifty. **A lower stage
   * never replaces a higher one**: the same guard `upsert_turn_notice` keeps,
   * so a window that walks back from rejected to a warning holds the line it
   * already drew.
   */
  const upsertNotice = (key: string, stage: number, notice: Notice): void => {
    const held = noticeStages.get(key);
    if (held !== undefined && stage < held) return;
    noticeStages.set(key, stage);
    for (let at = units.length - 1; at >= 0; at -= 1) {
      const unit = units[at];
      if (unit?.kind !== 'notice' || unit.key !== key) continue;
      units[at] = { kind: 'notice', key, notice };
      return;
    }
    push({ kind: 'notice', key, notice });
  };

  /**
   * Hang the harness's line about an image on the call that read it.
   *
   * The note arrives as a user frame right after the result, while the call
   * is still pending, so the last pending call holding an image is the row
   * the note belongs to. A note no call holds draws as its own line instead.
   */
  const attachImageNote = (note: string): boolean => {
    for (let at = pending.length - 1; at >= 0; at -= 1) {
      const item = pending[at];
      if (item?.tag === 'call' && item.leaf.image !== null && item.leaf.imageNote === null) {
        item.leaf.imageNote = note;
        return true;
      }
    }
    return false;
  };

  /** Attach a continuation prompt to the compaction row it belongs under. */
  const attachContinuation = (text: string, key: string): void => {
    flushWork();
    const row = lastCompaction;
    const held = row === null ? undefined : units[row];
    if (row !== null && held !== undefined && held.kind === 'compaction') {
      units[row] = { ...held, summary: text };
      return;
    }
    // No boundary frame reached this fold, so the prompt's own row carries
    // what it has: the cut happened, whatever the wire said about it.
    units.push({
      kind: 'compaction',
      key,
      trigger: null,
      preTokens: null,
      postTokens: null,
      summary: text,
    });
    lastCompaction = units.length - 1;
  };

  /** The running totals across the distinct messages seen so far, or null before any. */
  const liveUsage = (): { input: number; output: number; read: number; written: number } | null => {
    if (usageByMessage.size === 0) return null;
    const totals = { input: 0, output: 0, read: 0, written: 0 };
    for (const held of usageByMessage.values()) {
      totals.input += held.input;
      totals.output += held.output;
      totals.read += held.read;
      totals.written += held.written;
    }
    return totals;
  };

  const flushWork = (): void => {
    const items = pending;
    pending = [];
    // The first item is the guard and the unit's name in one: it always
    // carries its own id, so no counter is invented here - one would move as
    // the turn grows, which is the remount this keying exists to stop.
    const first = items[0];
    if (first === undefined) return;
    units.push({
      kind: 'leaves',
      key: first.tag === 'card' ? `p-${first.card.id}` : first.key,
      rows: items,
    });
  };

  const push = (unit: Unit): void => {
    flushWork();
    units.push(unit);
  };

  for (const [at, frame] of frames.entries()) {
    // A sub-agent's frames are the SUBAGENTS surface's, not the chat's.
    if (isDispatched(frame)) continue;
    // Whether this call was open when the turn ENDED: one before a failing
    // frame is a call the failure abandoned, one before any result frame is a
    // call the turn closed on, and - when the caller says the turn's history
    // is CLOSED - any call the frames never answered, which is the restart's
    // unterminated call: the resumed turn never brings its result, and a fold
    // reading only boundaries leaves the row spinning forever. A call opened
    // after a boundary is one the CLI is still running.
    //
    // **One stated divergence from the terminal** (rule 24): its normal
    // turn-end sweep draws a still-open call COMPLETED, where this settles it
    // failed. The two can only disagree when a turn ends CLEANLY with a
    // foreground call unanswered - the interrupted shapes carry an error
    // result, where both sides already say failed - and failed is the honest
    // word for a call that never came back.
    const abandoned =
      (failedAt !== null && at < failedAt) || (resultAt !== null && at < resultAt) || ended;

    // Watched before anything else reads the frame: whatever a turn turns out
    // to be, the clock on its own rows is the only one a later row can report.
    if (typeof frame.timestamp === 'string' && frame.timestamp !== '') {
      endedAt = frame.timestamp;
      startedAt ??= frame.timestamp;
    }

    if (frame.type === 'system') {
      // The core's own line about the seat: a command's answer, or why one
      // did not run. Nothing the CLI emitted carries it, so it is marked with
      // a subtype of its own rather than folded out of anything here.
      if (frame.subtype === 'forge_notice') {
        push({
          kind: 'notice',
          key: keyOf(at, frame, 'notice'),
          notice: { severity: noticeSeverity(frame.severity), text: str(frame, 'text') ?? '' },
        });
        continue;
      }
      // The CLI's own retry report, after a call it refused. One row per run
      // - a later attempt rewrites this line rather than drawing beside it -
      // and the words are the terminal's own, so a 429 storm reads as a line
      // saying why rather than as a stall.
      if (frame.subtype === 'api_retry') {
        const attempt = typeof frame.attempt === 'number' ? frame.attempt : null;
        const cap = typeof frame.max_retries === 'number' ? frame.max_retries : null;
        const delay = typeof frame.retry_delay_ms === 'number' ? frame.retry_delay_ms : null;
        if (attempt !== null && cap !== null && delay !== null) {
          const status = typeof frame.error_status === 'number' ? frame.error_status : null;
          upsertNotice('api-retry', NOTICE_STAGE.warning, {
            severity: 'warning',
            text: `API retry after ${retryLabel(frame.error)}${status === null ? '' : ` HTTP ${status}`}`,
            chip: `attempt ${attempt} / ${cap}`,
            sub: `retrying in ${retryDelay(delay)}`,
          });
        }
        continue;
      }
      // The counter arrives as a subtype of its own, and the wire's running
      // value restarts at every thinking block - so a turn's estimate is the
      // sum of its deltas rather than the last absolute one. Read before the
      // other system arms, because every one of them continues.
      if (frame.subtype === 'thinking_tokens') {
        const delta = frame.estimated_tokens_delta;
        if (typeof delta === 'number') thinking = (thinking ?? 0) + delta;
        continue;
      }
      if (frame.subtype === 'stop_hook_summary') {
        // The wire's own names, not the Rust fields they stand for: the
        // message renames both on the way out (`hookCount`, `hookInfos`), so a
        // read of `actions`/`hook_infos` is always undefined and the chip has
        // never drawn on a real session.
        const actions = typeof frame.hookCount === 'number' ? frame.hookCount : 0;
        if (actions > 0) {
          push({
            kind: 'hooks',
            key: typeof frame.uuid === 'string' ? frame.uuid : `hooks-${at}`,
            actions,
            infos: (Array.isArray(frame.hookInfos) ? frame.hookInfos : []).map((info) => ({
              command: str(info, 'command') ?? '',
              ...(typeof obj(info)['durationMs'] === 'number'
                ? { durationMs: obj(info)['durationMs'] as number }
                : {}),
            })),
            // The errors name no hook, so they are carried beside the hook
            // rows rather than against one of them.
            errors: (Array.isArray(frame.hookErrors) ? frame.hookErrors : []).filter(
              (error): error is string => typeof error === 'string',
            ),
          });
        }
        continue;
      }
      if (frame.subtype === 'compact_boundary') {
        const metadata = obj(frame.compact_metadata);
        // The server's serialization, not the CLI's disk spelling
        // (`preTokens`): this fold has no link to the type it reads, so a
        // rename would stop matching in silence.
        const count = (key: string): number | null => {
          const held = metadata[key];
          return typeof held === 'number' ? held : null;
        };
        push({
          kind: 'compaction',
          key: keyOf(at, frame, 'compaction'),
          trigger: str(metadata, 'trigger'),
          preTokens: count('pre_tokens'),
          postTokens: count('post_tokens'),
          summary: null,
        });
        lastCompaction = units.length - 1;
        continue;
      }
      // A hook's own lifecycle: one row per RUN, because the frames are one
      // hook's start, its interim output and its ending - the later frames
      // rewrite the run's own row rather than drawing beside it. The run lands
      // where it fired, so a hook that fired between two calls sits among them.
      if (
        frame.subtype === 'hook_started' ||
        frame.subtype === 'hook_progress' ||
        frame.subtype === 'hook_response'
      ) {
        const run = str(frame, 'hook_id');
        // A frame with no id can be paired with no other, so it draws as itself
        // rather than being folded onto a run it may not belong to.
        const key = run === null ? keyOf(at, frame, 'hook') : `hook-${run}`;
        if (run !== null && rewriteHook(key, hookRun(frame))) continue;
        pending.push({ tag: 'hook', key, run: hookRun(frame) });
        continue;
      }
      continue;
    }

    // A rate-limit window's state transition, as the terminal draws it: one
    // notice per incident - the window's type and its reset bucket - so a
    // later frame in the same window rewrites this line rather than stacking
    // beside it, and a new window opens one of its own. Allowed and unknown
    // statuses draw nothing, which is the terminal's own neutral rather than
    // a drop (`app/events/rate_limit.rs` routes them to no notice).
    if (frame.type === 'rate_limit_event') {
      const info = obj(frame.rate_limit_info);
      const status = str(info, 'status');
      if (status === 'allowed_warning' || status === 'rejected') {
        const rejected = status === 'rejected';
        upsertNotice(
          rateLimitNoticeKey(info),
          rejected ? NOTICE_STAGE.rejected : NOTICE_STAGE.warning,
          {
            severity: rejected ? 'error' : 'warning',
            text: formatRateLimitSummary(info),
          },
        );
      }
      continue;
    }

    if (frame.type === 'assistant' && typeof frame.message?.model === 'string') {
      model = frame.message.model;
    }
    // Every assistant message carries the counters of its own call, which is
    // what the running row counts up from. A frame with no usage block adds
    // nothing; a block that is there lands as its message's own counters,
    // zero or not, and a later frame for the same message replaces them.
    if (frame.type === 'assistant' && frame.message?.usage !== undefined) {
      const usage = obj(frame.message.usage);
      const figure = (key: string): number => (typeof usage[key] === 'number' ? usage[key] : 0);
      // A frame with no id cannot be matched to its repeats, so it keys on
      // itself; every captured frame carries one.
      usageByMessage.set(
        typeof frame.message.id === 'string'
          ? frame.message.id
          : typeof frame.uuid === 'string'
            ? frame.uuid
            : `f${at}`,
        {
          input: figure('input_tokens'),
          output: figure('output_tokens'),
          read: figure('cache_read_input_tokens'),
          written: figure('cache_creation_input_tokens'),
        },
      );
    }

    if (frame.type === 'result') {
      sawResult = true;
      flushWork();
      units.push({
        kind: 'report',
        info: reportOf(frame, model, thinking, endedAt),
        key: typeof frame.uuid === 'string' ? frame.uuid : `result-${at}`,
      });
      // The failure is a row of its own rather than a mark on the report: the
      // terminal states it the same way, as a line under the turn it belongs
      // to, and the report row is left saying only what the turn spent.
      const failure = turnFailure(frame);
      if (failure !== null) {
        units.push({ kind: 'notice', key: keyOf(at, frame, 'failure'), notice: failure });
      }
      thinking = null;
      continue;
    }

    if (frame.type === 'error') {
      // The string is all the turn leaves: no result frame follows, so there
      // is no report row to hang it on. The terminal surfaces the same string
      // on the same frame, for the same reason.
      const said = str(frame, 'error');
      if (said !== null && said.trim() !== '') {
        push({
          kind: 'notice',
          key: keyOf(at, frame, 'error'),
          notice: { severity: 'error', text: said },
        });
      }
      continue;
    }

    if (frame.type !== 'assistant' && frame.type !== 'user') continue;

    // What the frame attached, read before its blocks are walked: the wire
    // puts the thing AFTER the words it came with, and they are one row - so
    // the words are pushed with it rather than the attachment following them
    // as a row of its own.
    const files = frame.type === 'user' ? attachmentsOf(blocksOf(frame.message?.content)) : [];
    let tookFiles = false;

    for (const [blockAt, block] of blocksOf(frame.message?.content).entries()) {
      if (block.type === 'text' && typeof block.text === 'string') {
        if (frame.type === 'user') {
          const stripped = stripEscapes(block.text);
          // The local-command family draws nothing at all: it is the reader's
          // typing in the launch terminal, and the terminal's own chat filters
          // the same heads. A decided ignore, not a dropped frame.
          if (isLocalCommand(stripped)) continue;
          // A background task's end arrives as a user frame nobody typed -
          // the CLI's `<task-notification>` envelope - and draws as its own
          // summary line, never as the raw XML and never as the reader's turn
          // (#1680). No summary parsed, no claim: it falls through and draws
          // as itself, the default rule 25 keeps.
          const taskEnd = taskNotificationOf(stripped);
          if (taskEnd !== null) {
            push({
              kind: 'notice',
              key: keyOf(at, frame, blockAt),
              notice: { severity: 'info', text: taskEnd },
            });
            continue;
          }
          // The harness nudging the model after an invisible response: the
          // page's own line, never the raw bracket (#1858). A bracket this
          // does not recognize falls through and draws as itself.
          if (noOutputNudge(stripped)) {
            push({
              kind: 'notice',
              key: keyOf(at, frame, blockAt),
              notice: {
                severity: 'info',
                text: 'no visible output - the harness asked the agent to continue',
              },
            });
            continue;
          }
          const envelope = inbound(stripped, self);
          if (envelope !== null) {
            if (envelope.kind === 'peer') {
              // A header with no id leaves nothing in the data to name the
              // card by, so the frame and block stand in - position, but a
              // stable one, where a counter would move as the turn grows.
              pending.push({
                tag: 'card',
                card: {
                  ...envelope.card,
                  id: envelope.card.id !== '' ? envelope.card.id : keyOf(at, frame, blockAt),
                },
              });
            } else if (envelope.kind === 'inbound') {
              // A delivery by cron, Slack or Gotify joins the work as a row
              // of its own kind, the way a family's calls do - one shape for
              // every row, so a new external kind is a row and a glyph.
              //
              // A cron fire is titled by the SCHEDULE when the frame that
              // delivered it named one: the prose carries the prompt alone, so
              // the name is the one thing only the frame's own pairing can
              // say.
              const named = envelope.inbound === 'cron' ? cronNames.nameFor(frame.uuid) : null;
              pending.push({
                tag: 'inbound',
                kind: envelope.inbound,
                key: keyOf(at, frame, blockAt),
                title: named ?? envelope.title,
                body: envelope.body,
                elevated: envelope.elevated,
              });
            } else {
              push({ kind: 'notice', key: keyOf(at, frame, blockAt), notice: envelope.notice });
            }
            continue;
          }
          // The harness talking to the model is not the reader talking: the
          // reminder draws as a line of its own, so no turn claims words
          // nobody typed.
          if (isSkillReminder(stripped)) {
            push({
              kind: 'notice',
              key: keyOf(at, frame, blockAt),
              notice: { severity: 'info', text: stripped },
            });
            continue;
          }
          // Same rule for the frames nobody typed that arrive as the
          // reader's: a loaded skill's body and a compaction's continuation
          // prompt. Each rides the row it belongs to rather than the
          // reader's own.
          const skill = skillBody(stripped);
          const carried = skill?.name ?? headingNameOf(stripped);
          if (carried !== null) {
            if (attachSkillBody(carried, skill?.body ?? stripped.trim())) continue;
            // Unclaimed: a body with the CLI's plumbing line still gets a row
            // of its own rather than being dropped; a heading frame is an
            // ordinary user frame and falls through as it always did.
            if (skill !== null) {
              push({
                kind: 'skill',
                key: keyOf(at, frame, blockAt),
                name: skill.name,
                body: skill.body,
              });
              continue;
            }
          }
          if (isContinuation(stripped)) {
            attachContinuation(stripped, keyOf(at, frame, blockAt));
            continue;
          }
          // The harness's own line about the image the call above it just
          // read: it is the picture's caption on that call's row, not a turn
          // of the reader's own.
          const note = imageNoteOf(stripped);
          if (note !== null) {
            if (!attachImageNote(note)) {
              push({
                kind: 'notice',
                key: keyOf(at, frame, blockAt),
                notice: { severity: 'info', text: note },
              });
            }
            continue;
          }
          // **The mark the CLI gives every injected frame** (#1543), read LAST
          // on purpose: every family that claims a reader-shaped frame it did
          // not write sits above this - the reminder, a skill's body, a
          // compaction's continuation, an image's caption - and each of those
          // frames carries the mark too, so a mark read earlier would steal
          // them from their own rows. What reaches here is what none of them
          // claimed: a skill's body arriving with neither the plumbing line
          // nor a matching heading, which rides the first call still waiting
          // for one - named by position the way the CLI injects it, right
          // after the call that loaded the skill. With no call waiting it
          // still draws, as a notice - never as the reader's own turn.
          const body = stripped.trim();
          if (carried === null && body !== '' && frame.isSynthetic === true) {
            if (attachSkillBody(null, body)) continue;
            push({
              kind: 'notice',
              key: keyOf(at, frame, blockAt),
              notice: { severity: 'info', text: stripped },
            });
            continue;
          }
        }
        if (frame.type === 'user') {
          // The attachments ride the frame's first turn of words: a second
          // text block in the same frame is the same reader saying more, not
          // the same file sent twice.
          const note = noteOf(frame);
          push({
            kind: 'user',
            key: keyOf(at, frame, blockAt),
            text: block.text,
            files: tookFiles ? [] : files,
            ...(note === undefined ? {} : { note }),
          });
          tookFiles = true;
        } else {
          push({ kind: 'text', key: keyOf(at, frame, blockAt), text: block.text });
        }
        continue;
      }
      if (block.type === 'thinking' && typeof block.thinking === 'string') {
        // An empty one draws nothing, the way the terminal skips it: the row's
        // whole point is the words it carries.
        if (block.thinking.trim() !== '') {
          // Not `push`: a thought is commentary ON the work rather than a
          // separator between pieces of it, so nothing is flushed here. The
          // stretch stays whole across it, and the thought takes its place in
          // the arrival order like any other row.
          pending.push({ tag: 'thought', key: keyOf(at, frame, blockAt), text: block.thinking });
        }
        continue;
      }

      if (block.type === 'queued_command') {
        const words = queuedText(block.prompt);
        if (isCompletion(block, words)) continue;
        // **A queued prompt can be somebody else's words.** Half the queued
        // rows in this machine's transcripts are a peer envelope, and the
        // envelope is a row of its own rather than the reader's - the same
        // check the text arm above runs, for the same reason it runs it.
        const envelope = inbound(stripEscapes(words), self);
        if (envelope !== null) {
          if (envelope.kind === 'peer') {
            pending.push({
              tag: 'card',
              card: {
                ...envelope.card,
                id: envelope.card.id !== '' ? envelope.card.id : keyOf(at, frame, blockAt),
              },
            });
          } else if (envelope.kind === 'inbound') {
            pending.push({
              tag: 'inbound',
              kind: envelope.inbound,
              key: keyOf(at, frame, blockAt),
              title: envelope.title,
              body: envelope.body,
              elevated: envelope.elevated,
            });
          } else {
            push({ kind: 'notice', key: keyOf(at, frame, blockAt), notice: envelope.notice });
          }
          continue;
        }
        // Otherwise it is a turn the reader took, so what the frame attached
        // rides it exactly as it rides one they typed.
        push({
          kind: 'user',
          key: keyOf(at, frame, blockAt),
          text: words,
          files: tookFiles ? [] : files,
        });
        tookFiles = true;
        continue;
      }

      if (block.type === 'tool_use' || block.type === 'server_tool_use') {
        const name = typeof block.name === 'string' ? block.name : 'tool';
        const id = typeof block.id === 'string' ? block.id : '';
        if (isMonitor(name)) continue;

        const card = outbound(
          name,
          block.input,
          self,
          results.get(id),
          id !== '' ? id : keyOf(at, frame, blockAt),
        );
        if (card !== null) {
          pending.push({ tag: 'card', card });
          continue;
        }
        if (isQuestion(name)) {
          const card = questionCard(
            block.input,
            records.get(id),
            id !== '' ? `q-${id}` : keyOf(at, frame, blockAt),
          );
          if (card !== null) push(card);
          continue;
        }
        const leaf = leafOf(
          id,
          name,
          block.input,
          results.get(id),
          records.get(id),
          tasks.get(id),
          abandoned,
        );
        if (name.toLowerCase() === 'skill') {
          // A skill's body follows its call as a user frame; the claim is
          // recorded here, and the body attaches to this row when it arrives.
          const want = str(obj(block.input), 'skill');
          if (want !== null) skillCalls.push({ want, leaf });
        }
        pending.push({
          tag: 'call',
          key: id !== '' ? `c-${id}` : keyOf(at, frame, blockAt),
          leaf,
        });
        continue;
      }

      if (block.type === 'image' || block.type === 'document') {
        // Read by the frame rather than here: the attachment is part of the
        // turn the words above it opened, not a row of its own.
        continue;
      }
    }

    // A frame that attached something and said nothing is still a turn the
    // reader took, and the row draws what they sent rather than nothing.
    if (frame.type === 'user' && !tookFiles && files.length > 0) {
      push({ kind: 'user', key: keyOf(at, frame, 'files'), text: '', files });
    }
  }

  flushWork();
  // A live turn that has not settled draws its own row from what its frames
  // already carry: the stamps give the span so far, the assistant messages'
  // usage gives the token side, the counter frames give thinking. The
  // cumulative cost is settle-only, so the row has no segment for it until
  // the Result lands.
  if (live && !sawResult) {
    const held = liveUsage();
    units.push({
      kind: 'report',
      key: 'live-report',
      info: {
        running: true,
        failed: false,
        duration_ms: spanMs(startedAt, endedAt),
        api_ms: null,
        ended_at_utc: endedAt,
        model,
        thinking_tokens: thinking,
        input_tokens: held === null ? null : held.input,
        output_tokens: held === null ? null : held.output,
        cache_read_tokens: held === null ? null : held.read,
        cache_written_tokens: held === null ? null : held.written,
        session_cost_usd: null,
      },
    });
  }
  return units;
}

/** The wall clock between two stamps, or null when either is not a time. */
function spanMs(start: string | null, end: string | null): number | null {
  if (start === null || end === null) return null;
  const from = Date.parse(start);
  const to = Date.parse(end);
  return Number.isFinite(from) && Number.isFinite(to) ? Math.max(0, to - from) : null;
}
