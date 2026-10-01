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
 * Four places it deliberately differs from the terminal, and each is the
 * mockup's own drawing rather than a liberty:
 *
 * - a mutation folds as an `edit` family inside the run instead of breaking
 *   it;
 * - a question the assistant asked is a card rather than a call;
 * - an envelope that is not agent traffic is a notice rather than the reader's
 *   own turn;
 * - a monitor is not in the conversation at all, because the inspector is its
 *   surface.
 *
 * **And a dispatched agent's frames are not the conversation either.** A
 * sub-agent's prose and calls belong to the SUBAGENTS surface, and drawn here
 * they duplicate that surface inside every turn that used one.
 */

import {
  aggregateStatus,
  labelOf,
  rowOf,
  taskStatus,
  type CallStatus,
  type KindRow,
} from './families';
import { blocksOf, leafOf, type BackgroundTask, type Block, type ToolLeaf } from './leaves';
import { stripEscapes } from './text';

/** One question the assistant asked, with what was answered. */
export interface AnsweredQuestion {
  question: string;
  picked_labels: string[];
  typed_note: string | null;
}

/** One family's calls inside a group. */
export interface FamilyLeaves {
  /** The class the row belongs to, which is what a view picks its glyph from. */
  row: KindRow;
  /** The word the row draws. */
  label: string;
  calls: ToolLeaf[];
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
}

/** The lane a peer message draws on: the traffic's own three words. */
export type MessageKind = 'ask' | 'message' | 'reply';

/** The reader's own seat, for the two facts a message row compares against. */
export interface Self {
  org: string;
  project: string;
}

/** One peer message, as the envelope it arrived in or the call that sent it. */
export interface PeerCard {
  /**
   * The wire's own id for the message: the envelope's, or the `tool_use` id of
   * the call that sent it. It is what a view names the group by, so a handle
   * never has to be derived from a name two messages can share.
   */
  id: string;
  peer: string;
  body: string;
  /**
   * Which lane it draws on, never which way it travelled: direction is not
   * drawn, so a question this session sent and one it received both read
   * `ask`.
   */
  kind: MessageKind;
  /** Whether the counterparty is in this project, which the row's mark says. */
  here: boolean;
  /** The counterparty's org, shown only when it is not the reader's own. */
  org: string | null;
  /**
   * What became of the message: a send with no answer yet is still out, one
   * whose result came back in error did not arrive, and an envelope that
   * arrived is done.
   */
  status: CallStatus;
}

/** One lane of a message group: its kind, and the messages that arrived as it. */
export interface MessageLane {
  kind: MessageKind;
  cards: PeerCard[];
}

/** What one turn's hooks did. */
export interface HookInfo {
  command: string;
  durationMs?: number;
}

/**
 * What a settled turn did: its clock, its API time, and the tokens and cost
 * the CLI reported. Every field is what the frame carried, and an absent one
 * draws as a dash rather than as a zero.
 */
export interface TurnInfo {
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
 * is re-folded whole on every frame: a thinking row can land ABOVE units
 * already drawn, and a list keyed by position remounts everything below it,
 * closing whatever the reader had open. Keyed by this, the row is moved.
 */
export type Unit =
  /** A turn the user wrote, with whatever they attached to it. */
  | { kind: 'user'; key: string; text: string; files: AttachedFile[] }
  /** Prose the assistant wrote. */
  | { kind: 'text'; key: string; text: string }
  /**
   * What the model thought before it said anything.
   *
   * The wire carries it as its own block and nothing drew it, which is a frame
   * dropped rather than a shape chosen: the row is drawn collapsed, carrying
   * the thinking's own first words.
   */
  | { kind: 'thinking'; key: string; text: string }
  /** A maximal run of consecutive tool calls, drawn as one group. */
  | { kind: 'group'; key: string; families: FamilyLeaves[]; status: CallStatus }
  | { kind: 'question'; key: string; asked: AnsweredQuestion[] }
  /**
   * A run of peer messages, drawn as one group with a lane per kind.
   *
   * A lone message is a group of one, which is the one place this shape
   * departs from the terminal: its own fold holds a messaging group back until
   * it holds two.
   */
  | { kind: 'messages'; key: string; lanes: MessageLane[]; status: CallStatus }
  | { kind: 'notice'; key: string; notice: Notice }
  | { kind: 'hooks'; key: string; actions: number; infos: HookInfo[] }
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
  parent_tool_use_id?: unknown;
  hookCount?: unknown;
  hookInfos?: unknown;
  task_id?: unknown;
  tool_use_id?: unknown;
  is_backgrounded?: unknown;
  patch?: unknown;
  summary?: unknown;
  status?: unknown;
  estimated_tokens_delta?: unknown;
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
  state?: unknown;
  timestamp?: unknown;
  message?: { content?: unknown; model?: unknown; stop_reason?: unknown };
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

/** The project an address names: `project/label` is a worker, a bare name an own agent. */
function projectOf(address: string): string {
  const cut = address.lastIndexOf('/');
  return cut === -1 ? address : address.slice(0, cut);
}

/**
 * One peer message, with the two facts its row compares against the reader.
 *
 * `self` is the seat the page draws, and `null` when the fold runs without one
 * (the turn-opening probe): with no reader to compare against, the
 * counterparty draws as in this project and carries no org tag.
 */
function peerCard(one: {
  id: string;
  peer: string;
  body: string;
  kind: MessageKind;
  org: string | null;
  self: Self | null;
  status: CallStatus;
}): PeerCard {
  return {
    id: one.id,
    peer: one.peer,
    body: one.body,
    kind: one.kind,
    here: one.self === null || projectOf(one.peer) === one.self.project,
    org: one.org !== null && (one.self === null || one.org !== one.self.org) ? one.org : null,
    status: one.status,
  };
}

/** What a send's own result says became of it, and a send with none is still out. */
function messageStatus(result: Block | undefined): CallStatus {
  if (result === undefined) return 'in_progress';
  return result.is_error === true ? 'failed' : 'completed';
}

/** What an envelope's prose turned into: a peer message, or a line nobody typed. */
type Envelope = { kind: 'peer'; card: PeerCard } | { kind: 'notice'; notice: Notice };

/**
 * A Slack id is not a name to print.
 *
 * The server's own heuristic, ported as it stands: an uppercase initial and
 * nothing but uppercase or digits after it, with no length floor and no
 * letter restriction. It is a heuristic and the server says so - an all-caps
 * channel name matches it and loses its `#` - but a client that tightened it
 * for looks would drop the author clause on a `B…` bot id where the terminal
 * drops it, which is one more rule that is not the rule it ports.
 */
function isSlackId(value: string): boolean {
  const [first, ...rest] = value;
  if (first === undefined || !/^[A-Z]$/.test(first)) return false;
  return rest.every((letter) => /^[A-Z0-9]$/.test(letter));
}

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
 * The header is everything between `[` and the first `]`, which matters for
 * the two kinds that carry a trailer: a question ends `- reply with ...` and
 * a reply ends `to your earlier ask`, both inside the brackets.
 */
function inbound(text: string, self: Self | null): Envelope | null {
  if (!text.startsWith('[')) return null;
  const close = text.indexOf(']');
  if (close === -1) return null;
  const header = text.slice(1, close);
  const tail = text.slice(close + 1);
  // The peer wrappers land their body tail `]\n\n`; the notice kinds have
  // their own spacing and are read below.
  const body = tail.startsWith('\n\n') ? tail.slice(2) : '';

  for (const [prefix, lane] of [
    ['Question id=', 'ask'],
    ['Message id=', 'message'],
    ['Reply id=', 'reply'],
  ] as const) {
    if (!header.startsWith(prefix)) continue;
    const spec = header.slice(prefix.length);
    const at = spec.indexOf(' from agent ');
    if (at === -1) return null;
    const who = sender(spec.slice(at + ' from agent '.length));
    if (who === null) return null;
    return {
      kind: 'peer',
      card: peerCard({
        id: spec.slice(0, at),
        peer: who.from,
        body,
        kind: lane,
        org: who.org,
        self,
        status: 'completed',
      }),
    };
  }

  if (header.startsWith('Ask id=')) {
    const to = after(header.slice('Ask id='.length), ' to agent ');
    if (to === null || !header.includes('failed to deliver:')) return null;
    const who = sender(to);
    if (who === null) return null;
    const reason = after(to, 'failed to deliver:') ?? '';
    return {
      kind: 'notice',
      notice: {
        severity: 'warning',

        text: `'${who.from}' (${who.org}) failed to deliver: ${reason.trim()}`.trimEnd(),
      },
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
      kind: 'notice',
      notice: {
        severity: priority >= 5 ? 'warning' : 'info',

        text: `app '${parts[0]}' \u{b7} priority ${priority}: ${title}\n${message}`.trimEnd(),
      },
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
      kind: 'notice',
      notice: { severity: 'info', text: `${head}: ${said}`.trimEnd() },
    };
  }

  if (header === 'Cron') {
    return { kind: 'notice', notice: { severity: 'info', text: body } };
  }

  return null;
}

/**
 * The peer card an outbound call draws, when it is one.
 *
 * **Each name carries its own input shape, and a call with no target is not a
 * card at all.** A reply goes to whoever asked, so it carries no target: the
 * server's own answer is `None`, which draws the call as the tool row it is
 * rather than as a peer block with a nameless peer.
 */
function outbound(
  name: string,
  input: unknown,
  self: Self | null,
  result: Block | undefined,
  id: string,
): PeerCard | null {
  const fields = obj(input);
  const ask =
    name.endsWith('__ask') || name.endsWith('__ask_agent') || name.endsWith('__workers__ask');
  const lane: MessageKind = ask ? 'ask' : 'message';
  const status = messageStatus(result);

  if (name === 'mcp__forge__agents__ask' || name === 'mcp__forge__agents__tell') {
    const peer = address(fields);
    if (peer === null) return null;
    const body = str(fields, ask ? 'prompt' : 'message') ?? '';
    return peerCard({ id, peer, body, kind: lane, org: null, self, status });
  }
  if (name === 'mcp__forge__peers__ask_agent' || name === 'mcp__forge__peers__tell_agent') {
    const peer = str(fields, 'target');
    if (peer === null) return null;
    const body = str(fields, ask ? 'prompt' : 'message') ?? '';
    return peerCard({ id, peer, body, kind: lane, org: null, self, status });
  }
  if (name === 'mcp__forge__workers__ask' || name === 'mcp__forge__workers__tell') {
    const peer = str(fields, 'label');
    if (peer === null) return null;
    const body = str(fields, ask ? 'question' : 'message') ?? '';
    return peerCard({ id, peer, body, kind: lane, org: null, self, status });
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
function questionCard(input: unknown, answer: unknown, key: string): Unit {
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
    const typed =
      values.find((value) => value !== '' && !labels.includes(value)) ??
      str(obj(annotations[text]), 'notes') ??
      null;

    asked.push({ question: text, picked_labels: picked, typed_note: typed === '' ? null : typed });
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
function taskLine(summary: string | null, wire: string | null): BackgroundTask['note'] {
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
 */
export function fold(
  messages: readonly unknown[],
  cwd: string | null = null,
  self: Self | null = null,
): Unit[] {
  const frames = messages as Frame[];
  /** Every result the turn holds, by the call it answers. */
  const results = new Map<string, ReturnType<typeof blocksOf>[number]>();
  /** What each question was answered with, by the call that asked it. */
  const answers = new Map<string, unknown>();
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
  /** What the wire reported about each backgrounded call, by call. */
  const tasks = new Map<string, BackgroundTask>();
  /** The call a task belongs to, which the frames that carry one name. */
  const owners = new Map<string, string>();
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
        const held: BackgroundTask = tasks.get(owner) ?? {
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
        // The record of what was answered rides beside the result, keyed the
        // same way, and a question is drawn from it rather than from the
        // result's own text - which is the CLI saying it was answered.
        if (frame.tool_use_result !== undefined) {
          answers.set(block.tool_use_id, frame.tool_use_result);
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
  let run: Array<{ row: KindRow; label: string; leaf: ToolLeaf; key: string }> = [];
  let peers: PeerCard[] = [];
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

  const flushRun = (): void => {
    if (run.length === 0) return;
    const calls = run;
    run = [];
    const families: FamilyLeaves[] = [];
    for (const entry of calls) {
      const held = families.find(
        (family) => family.label === entry.label && family.row.kind === entry.row.kind,
      );
      if (held === undefined)
        families.push({ row: entry.row, label: entry.label, calls: [entry.leaf] });
      else held.calls.push(entry.leaf);
    }
    units.push({
      kind: 'group',
      key: calls[0]?.key ?? `g-${units.length}`,
      families,
      status: aggregateStatus(calls.map((entry) => entry.leaf.status)),
    });
  };

  const flushPeers = (): void => {
    const cards = peers;
    peers = [];
    if (cards.length === 0) return;
    // One lane per KIND, first seen first - not one per run. The terminal's own
    // tally draws a lane per kind over the whole group, and a lane's word is
    // what a view opens its leaves by: two runs of the same kind would give two
    // lanes the same word, which a keyed list refuses at mount.
    const lanes: MessageLane[] = [];
    const seen = new Map<MessageKind, MessageLane>();
    for (const card of cards) {
      const held = seen.get(card.kind);
      if (held !== undefined) {
        held.cards.push(card);
        continue;
      }
      const lane: MessageLane = { kind: card.kind, cards: [card] };
      seen.set(card.kind, lane);
      lanes.push(lane);
    }
    units.push({
      kind: 'messages',
      key: `p-${cards[0]?.id ?? `batch-${units.length}`}`,
      lanes,
      status: aggregateStatus(cards.map((c) => c.status)),
    });
  };

  const push = (unit: Unit): void => {
    flushRun();
    flushPeers();
    units.push(unit);
  };

  for (const [at, frame] of frames.entries()) {
    // A sub-agent's frames are the SUBAGENTS surface's, not the chat's.
    if (isDispatched(frame)) continue;
    // Whether this call was open when the turn failed: the ones before that
    // frame are the ones it abandoned, and a call the CLI opened after it is
    // one the CLI is still running.
    const abandoned = failedAt !== null && at < failedAt;

    // Watched before anything else reads the frame: whatever a turn turns out
    // to be, the clock on its own rows is the only one a later row can report.
    if (typeof frame.timestamp === 'string' && frame.timestamp !== '') {
      endedAt = frame.timestamp;
    }

    if (frame.type === 'system') {
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
          });
        }
        continue;
      }
      continue;
    }

    if (frame.type === 'assistant' && typeof frame.message?.model === 'string') {
      model = frame.message.model;
    }

    if (frame.type === 'result') {
      flushRun();
      flushPeers();
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
          const envelope = inbound(stripEscapes(block.text), self);
          if (envelope !== null) {
            if (envelope.kind === 'peer') {
              // A peer message is a row the CLI answered as a turn of its own,
              // so the run above it belongs to the turn before: it closes here.
              flushRun();
              peers.push(envelope.card);
            } else {
              push({ kind: 'notice', key: keyOf(at, frame, blockAt), notice: envelope.notice });
            }
            continue;
          }
        }
        if (frame.type === 'user') {
          // The attachments ride the frame's first turn of words: a second
          // text block in the same frame is the same reader saying more, not
          // the same file sent twice.
          push({
            kind: 'user',
            key: keyOf(at, frame, blockAt),
            text: block.text,
            files: tookFiles ? [] : files,
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
          // separator between pieces of it, so nothing is flushed here. The run
          // stays whole across the row, and so does a batch of peer traffic -
          // the row draws above both.
          units.push({ kind: 'thinking', key: keyOf(at, frame, blockAt), text: block.thinking });
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
            flushRun();
            peers.push(envelope.card);
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

        const card = outbound(name, block.input, self, results.get(id), id);
        if (card !== null) {
          flushRun();
          peers.push(card);
          continue;
        }
        if (isQuestion(name)) {
          push(
            questionCard(
              block.input,
              answers.get(id),
              id !== '' ? `q-${id}` : keyOf(at, frame, blockAt),
            ),
          );
          continue;
        }
        flushPeers();
        run.push({
          row: rowOf(name),
          label: labelOf(name),
          key: id !== '' ? `c-${id}` : keyOf(at, frame, blockAt),
          leaf: leafOf(id, name, block.input, results.get(id), cwd, tasks.get(id), abandoned),
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

  flushRun();
  flushPeers();
  return units;
}
