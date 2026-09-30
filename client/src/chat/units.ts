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

import { aggregateStatus, labelOf, rowOf, type CallStatus, type KindRow } from './families';
import { blocksOf, leafOf, type Block, type ToolLeaf } from './leaves';
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

/** A peer message, as the envelope it arrived in or the call that sent it. */
export interface PeerCard {
  peer: string;
  body: string;
  /** True when it arrived rather than was sent. */
  inbound: boolean;
  /** `question`, `message` or `reply` inbound; `ask` or `tell` outbound. */
  kind: string;
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

/** One thing a view draws, in the order the conversation produced it. */
export type Unit =
  /** A turn the user wrote, with whatever they attached to it. */
  | { kind: 'user'; text: string; files: AttachedFile[] }
  /** Prose the assistant wrote. */
  | { kind: 'text'; text: string }
  /** A maximal run of consecutive tool calls, drawn as one group. */
  | { kind: 'group'; families: FamilyLeaves[]; status: CallStatus }
  | { kind: 'question'; asked: AnsweredQuestion[] }
  | { kind: 'peer'; card: PeerCard }
  /** Two or more consecutive peer messages, drawn as one group with a count. */
  | { kind: 'peers'; cards: PeerCard[] }
  | { kind: 'notice'; notice: Notice }
  | { kind: 'hooks'; key: string; actions: number; infos: HookInfo[] }
  /** What a settled turn did, under the work it did it with. */
  | { kind: 'report'; info: TurnInfo; key: string | null };

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
function inbound(text: string): Unit | null {
  if (!text.startsWith('[')) return null;
  const close = text.indexOf(']');
  if (close === -1) return null;
  const header = text.slice(1, close);
  const tail = text.slice(close + 1);
  // The peer wrappers land their body tail `]\n\n`; the notice kinds have
  // their own spacing and are read below.
  const body = tail.startsWith('\n\n') ? tail.slice(2) : '';

  for (const [prefix, kind] of [
    ['Question id=', 'question'],
    ['Message id=', 'message'],
    ['Reply id=', 'reply'],
  ] as const) {
    if (!header.startsWith(prefix)) continue;
    const rest = after(header.slice(prefix.length), ' from agent ');
    if (rest === null) return null;
    const who = sender(rest);
    if (who === null) return null;
    return { kind: 'peer', card: { peer: who.from, body, inbound: true, kind } };
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
function outbound(name: string, input: unknown): PeerCard | null {
  const fields = obj(input);
  const ask =
    name.endsWith('__ask') || name.endsWith('__ask_agent') || name.endsWith('__workers__ask');

  if (name === 'mcp__forge__agents__ask' || name === 'mcp__forge__agents__tell') {
    const peer = address(fields);
    if (peer === null) return null;
    return {
      peer,
      body: str(fields, name.endsWith('__ask') ? 'prompt' : 'message') ?? '',
      inbound: false,
      kind: ask ? 'ask' : 'tell',
    };
  }
  if (name === 'mcp__forge__peers__ask_agent' || name === 'mcp__forge__peers__tell_agent') {
    const peer = str(fields, 'target');
    if (peer === null) return null;
    return {
      peer,
      body: str(fields, ask ? 'prompt' : 'message') ?? '',
      inbound: false,
      kind: ask ? 'ask' : 'tell',
    };
  }
  if (name === 'mcp__forge__workers__ask' || name === 'mcp__forge__workers__tell') {
    const peer = str(fields, 'label');
    if (peer === null) return null;
    return {
      peer,
      body: str(fields, ask ? 'question' : 'message') ?? '',
      inbound: false,
      kind: ask ? 'ask' : 'tell',
    };
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
function questionCard(input: unknown, answer: unknown): Unit {
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
  return { kind: 'question', asked };
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
 * Fold a turn's messages into the units a view draws.
 *
 * A `turn` is one page's worth of conversation as the server cut it, so this
 * never has to decide where a turn begins: it decides how the blocks inside
 * one read.
 */
export function fold(messages: readonly unknown[], cwd: string | null = null): Unit[] {
  const frames = messages as Frame[];
  /** Every result the turn holds, by the call it answers. */
  const results = new Map<string, ReturnType<typeof blocksOf>[number]>();
  /** What each question was answered with, by the call that asked it. */
  const answers = new Map<string, unknown>();
  /**
   * Whether the turn failed, which is what the frame recording it says.
   *
   * A turn that fails answers no call that was still out when it did, so a
   * call left with nothing to settle it is one the CLI abandoned rather than
   * one still running.
   */
  let abandoned = false;
  for (const frame of frames) {
    // A dispatched agent's frames are not the conversation, and its verdict is
    // not the session's: a sub-agent's failed result says nothing about the
    // turn the parent is still running. The drawing loop skips these frames
    // the same way, so a pre-pass that read one would finalize calls the loop
    // never drew a row for.
    if (isDispatched(frame)) continue;
    // A fatal error is the CLI's last-gasp signal before teardown: no result
    // frame follows it, so it is the turn's own verdict just as a failed
    // result is.
    if (frame.type === 'error') abandoned = true;
    if (frame.type === 'result' && frame.is_error === true) abandoned = true;
    if (frame.type !== 'user') continue;
    for (const block of blocksOf(frame.message?.content)) {
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

  const units: Unit[] = [];
  let run: Array<{ row: KindRow; label: string; leaf: ToolLeaf }> = [];
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
      families,
      status: aggregateStatus(calls.map((entry) => entry.leaf.status)),
    });
  };

  const flushPeers = (): void => {
    const cards = peers;
    peers = [];
    if (cards.length === 1 && cards[0] !== undefined) units.push({ kind: 'peer', card: cards[0] });
    else if (cards.length > 1) units.push({ kind: 'peers', cards });
  };

  const push = (unit: Unit): void => {
    flushRun();
    flushPeers();
    units.push(unit);
  };

  for (const frame of frames) {
    // A sub-agent's frames are the SUBAGENTS surface's, not the chat's.
    if (isDispatched(frame)) continue;

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
            key: typeof frame.uuid === 'string' ? frame.uuid : 'hooks',
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
        key: typeof frame.uuid === 'string' ? frame.uuid : null,
      });
      // The failure is a row of its own rather than a mark on the report: the
      // terminal states it the same way, as a line under the turn it belongs
      // to, and the report row is left saying only what the turn spent.
      const failure = turnFailure(frame);
      if (failure !== null) units.push({ kind: 'notice', notice: failure });
      thinking = null;
      continue;
    }

    if (frame.type === 'error') {
      // The string is all the turn leaves: no result frame follows, so there
      // is no report row to hang it on. The terminal surfaces the same string
      // on the same frame, for the same reason.
      const said = str(frame, 'error');
      if (said !== null && said.trim() !== '') {
        push({ kind: 'notice', notice: { severity: 'error', text: said } });
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

    for (const block of blocksOf(frame.message?.content)) {
      if (block.type === 'text' && typeof block.text === 'string') {
        if (frame.type === 'user') {
          const envelope = inbound(stripEscapes(block.text));
          if (envelope !== null) {
            if (envelope.kind === 'peer') {
              // A peer message is a row the CLI answered as a turn of its own,
              // so the run above it belongs to the turn before: it closes here.
              flushRun();
              peers.push(envelope.card);
            } else {
              push(envelope);
            }
            continue;
          }
        }
        if (frame.type === 'user') {
          // The attachments ride the frame's first turn of words: a second
          // text block in the same frame is the same reader saying more, not
          // the same file sent twice.
          push({ kind: 'user', text: block.text, files: tookFiles ? [] : files });
          tookFiles = true;
        } else {
          push({ kind: 'text', text: block.text });
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
        const envelope = inbound(stripEscapes(words));
        if (envelope !== null) {
          if (envelope.kind === 'peer') {
            flushRun();
            peers.push(envelope.card);
          } else {
            push(envelope);
          }
          continue;
        }
        // Otherwise it is a turn the reader took, so what the frame attached
        // rides it exactly as it rides one they typed.
        push({ kind: 'user', text: words, files: tookFiles ? [] : files });
        tookFiles = true;
        continue;
      }

      if (block.type === 'tool_use' || block.type === 'server_tool_use') {
        const name = typeof block.name === 'string' ? block.name : 'tool';
        const id = typeof block.id === 'string' ? block.id : '';
        if (isMonitor(name)) continue;

        const card = outbound(name, block.input);
        if (card !== null) {
          flushRun();
          peers.push(card);
          continue;
        }
        if (isQuestion(name)) {
          push(questionCard(block.input, answers.get(id)));
          continue;
        }
        flushPeers();
        run.push({
          row: rowOf(name),
          label: labelOf(name),
          leaf: leafOf(id, name, block.input, results.get(id), cwd, abandoned),
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
      push({ kind: 'user', text: '', files });
    }
  }

  flushRun();
  flushPeers();
  return units;
}
