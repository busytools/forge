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
import { blocksOf, leafOf, type ToolLeaf } from './leaves';
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

/** A line the conversation carries that nobody typed. */
export interface Notice {
  severity: NoticeSeverity;
  /** Where it came from: `gotify`, `cron`, `slack`, `peer` or `worker`. */
  source: string;
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

/** One thing a view draws, in the order the conversation produced it. */
export type Unit =
  /** A turn the user wrote. */
  | { kind: 'user'; text: string }
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

/** The tools whose chat surface is a peer block rather than a call row. */
const PEER_TOOLS = new Set([
  'mcp__forge__agents__ask',
  'mcp__forge__agents__tell',
  // Replay-only names: a transcript written before the rename carries these.
  'mcp__forge__peers__ask_agent',
  'mcp__forge__peers__tell_agent',
  'mcp__forge__workers__ask',
  'mcp__forge__workers__tell',
]);

/** The tool a lifecycle block is drawn for, which the chat does not draw at all. */
function isMonitor(name: string): boolean {
  return name === 'Monitor';
}

/** Whether a call is a question the assistant asked. */
function isQuestion(name: string): boolean {
  return name === 'AskUserQuestion';
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
  isSidechain?: unknown;
  parent_tool_use_id?: unknown;
  actions?: unknown;
  hook_infos?: unknown;
  estimated_tokens_delta?: unknown;
  duration_ms?: unknown;
  duration_api_ms?: unknown;
  total_cost_usd?: unknown;
  usage?: unknown;
  tool_use_result?: unknown;
  state?: unknown;
  message?: { content?: unknown; model?: unknown; stop_reason?: unknown };
}

/** Whether a frame came from a dispatched agent rather than the session's own. */
function isDispatched(frame: Frame): boolean {
  // The wire's own marker for a sidechain row, and the one this corpus
  // actually carries: `parent_tool_use_id` is null on every top-level frame
  // measured, while `isSidechain` marks the rows a sub-agent wrote. A guard
  // written for a shape nobody produces reads as working and suppresses
  // nothing, which is worse than no guard at all.
  if (frame.isSidechain === true) return true;
  return typeof frame.parent_tool_use_id === 'string' && frame.parent_tool_use_id.trim() !== '';
}

/** The envelope text a user frame carries, when it is one this page knows. */
function inbound(text: string): Unit | null {
  const peer =
    /^\[(?:Question id=\S+|Message id=\S+|Reply id=\S+) from agent '([^']+)' \(org '[^']*'\)\]/;
  const question = /^\[Question id=\S+ from agent '([^']+)'/;
  const reply = /^\[Reply id=\S+ from agent '([^']+)'/;

  const head = peer.exec(text);
  if (head !== null) {
    const body = text.slice(head[0].length).trim();
    const kind = question.test(text) ? 'question' : reply.test(text) ? 'reply' : 'message';
    return { kind: 'peer', card: { peer: head[1] ?? '', body, inbound: true, kind } };
  }

  const gotify = /^\[Gotify - app '([^']*)', priority (\d+)\]\n?/.exec(text);
  if (gotify !== null) {
    return {
      kind: 'notice',
      notice: {
        severity: Number(gotify[2] ?? '0') >= 5 ? 'warning' : 'info',
        source: 'gotify',
        text: text.slice(gotify[0].length).trimEnd(),
      },
    };
  }

  const slack = /^\[Slack - workspace '([^']+)', (#[^\]\s]+)\]\s*\S*\s*\n?/.exec(text);
  if (slack !== null) {
    return {
      kind: 'notice',
      notice: {
        severity: 'info',
        source: 'slack',
        text: `${slack[1]} \u{b7} ${slack[2]}: ${text.slice(slack[0].length).trimEnd()}`,
      },
    };
  }

  const failed =
    /^\[(?:Ask|Tell) id=\S+ to agent '[^']+' \(org '[^']*'\) failed to deliver: (.*)\]$/s.exec(
      text,
    );
  if (failed !== null) {
    return {
      kind: 'notice',
      notice: { severity: 'warning', source: 'peer', text: text.slice(1, -1).trimEnd() },
    };
  }

  const spawn = /^\[Worker '([^']+)' failed to spawn: (.*)\]$/s.exec(text);
  if (spawn !== null) {
    return {
      kind: 'notice',
      notice: {
        severity: 'warning',
        source: 'worker',
        text: `'${spawn[1]}' failed to spawn: ${spawn[2] ?? ''}`.trimEnd(),
      },
    };
  }

  const cron = /^\[Cron(?: id=\S+)?\]\n?/.exec(text);
  if (cron !== null) {
    return {
      kind: 'notice',
      notice: { severity: 'info', source: 'cron', text: text.slice(cron[0].length).trimEnd() },
    };
  }

  return null;
}

/** The peer card an outbound call draws, when it is one. */
function outbound(name: string, input: unknown): PeerCard | null {
  if (!PEER_TOOLS.has(name)) return null;
  const fields = obj(input);
  const peer =
    str(fields, 'project') ??
    str(fields, 'target') ??
    str(fields, 'agent') ??
    str(fields, 'label') ??
    '';
  const body = str(fields, 'message') ?? str(fields, 'prompt') ?? str(fields, 'body') ?? '';
  const ask =
    name.endsWith('__ask') || name.endsWith('__ask_agent') || name.endsWith('__workers__ask');
  return { peer, body, inbound: false, kind: ask ? 'ask' : 'tell' };
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
function reportOf(frame: Frame, model: string | null, thinking: number | null): TurnInfo {
  const usage = obj(frame.usage);
  const count = (key: string): number | null => {
    const value = usage[key];
    return typeof value === 'number' ? value : null;
  };
  return {
    duration_ms: typeof frame.duration_ms === 'number' ? frame.duration_ms : null,
    api_ms: typeof frame.duration_api_ms === 'number' ? frame.duration_api_ms : null,
    ended_at_utc: null,
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
  for (const frame of frames) {
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

    if (frame.type === 'system') {
      if (frame.subtype === 'stop_hook_summary') {
        const actions = typeof frame.actions === 'number' ? frame.actions : 0;
        if (actions > 0) {
          push({
            kind: 'hooks',
            key: typeof frame.uuid === 'string' ? frame.uuid : 'hooks',
            actions,
            infos: (Array.isArray(frame.hook_infos) ? frame.hook_infos : []).map((info) => ({
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

    // The wire's running counter restarts at every thinking block, so a turn's
    // estimate is the sum of its deltas rather than the last absolute value.
    if (frame.type === 'system' || frame.type === 'assistant') {
      const delta = frame.estimated_tokens_delta;
      if (typeof delta === 'number') thinking = (thinking ?? 0) + delta;
    }

    if (frame.type === 'result') {
      flushRun();
      flushPeers();
      units.push({
        kind: 'report',
        info: reportOf(frame, model, thinking),
        key: typeof frame.uuid === 'string' ? frame.uuid : null,
      });
      thinking = null;
      continue;
    }

    if (frame.type !== 'assistant' && frame.type !== 'user') continue;

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
        push({ kind: frame.type === 'user' ? 'user' : 'text', text: block.text });
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
          leaf: leafOf(id, name, block.input, results.get(id), cwd),
        });
      }
    }
  }

  flushRun();
  flushPeers();
  return units;
}
