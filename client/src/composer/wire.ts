/**
 * What the core sends the composer, narrowed once at the boundary.
 *
 * `crates/forge-server/src/transport/wire.rs` is the sender: `composer` carries
 * the take, the notice, a compaction and a sign-in, and `pending_asks` carries
 * the prompts the seat is holding. Everything here is a narrowing rather than
 * a decision about how any of it looks - the gather into what the markup
 * wants is `view.ts`.
 */

import type { SessionSlot } from '../wire/types';
import { bindFrom, modeFrom, type Bind, type Mode } from './dictate-key';

/** What a take is doing, as `Phase` serialises. */
export type TakePhase = 'recording' | 'transcribing';

/** A take in flight, as its row draws it. */
export interface Take {
  phase: TakePhase;
  /** Newest reading last, each the peak over the window since the previous. */
  levels: number[];
  peakDb: number;
  /** Settled segments, and their total once the take closed. */
  progress: { done: number; total: number | null };
  /** How long the take has run, which `Instant` cannot carry across. */
  elapsedMs: number;
}

/** What a finished take left behind. */
export type Notice =
  | { kind: 'landed'; text: string; truncated: boolean }
  | { kind: 'line'; tone: string; text: string };

/** The sign-in a seat is waiting on, which makes the hint say which account. */
export interface SignIn {
  methodName: string;
  methodDescription: string;
}

/** What the seat's composer is doing, which a client cannot reconstruct. */
export interface ComposerState {
  take: Take | null;
  notice: Notice | null;
  compacting: boolean;
  signIn: SignIn | null;
  /** The push-to-talk key `forge.toml` configured, which is the one honoured. */
  bind: Bind;
  /** How a press of that key maps onto a take, from the same section. */
  mode: Mode;
}

/** One option a permission offers, with the action only the core may build on. */
export interface PermissionOption {
  optionId: string;
  name: string;
  /** What the option means, which its row draws as an icon. */
  kind: 'allow' | 'deny' | 'edit' | 'notes';
  /** The dispatch routing the core set for it, echoed back untouched. */
  action: Record<string, unknown>;
}

/** A permission as the core offers it. */
export interface PermissionRequest {
  toolId: string;
  /** The CLI's own name for the call, falling back to the tool's title. */
  title: string;
  /** What the call is about: a command, a path or a URL. */
  subject: string;
  /** The CLI's reason for asking, which the dock leads with when it has one. */
  reason: string | null;
  description: string | null;
  options: PermissionOption[];
}

/** One option a question offers. */
export interface QuestionOption {
  optionId: string;
  label: string;
  /** What choosing it means, which the row draws dim under the label. */
  description: string | null;
  /** What it would do, shown for the marked row while the reader decides. */
  preview: string | null;
}

/** A question as the core offers it. */
export interface QuestionRequest {
  toolId: string;
  header: string;
  question: string;
  /** Whether more than one option can be answered with, which the rows toggle on. */
  multiSelect: boolean;
  options: QuestionOption[];
  /** Which question this is, and how many the call asks. */
  index: number;
  total: number;
}

/** A held Slack post: what is waiting, and what it would send. */
export interface SlackDraft {
  /** The draft's own id, which is what an answer is addressed by. */
  id: string;
  workspace: string;
  conversationLabel: string;
  /** `null` posts a root message; a timestamp replies into that thread. */
  threadTs: string | null;
  text: string;
}

/** The prompt the seat is parked on. */
export type Ask =
  | { kind: 'permission'; request: PermissionRequest }
  | { kind: 'question'; request: QuestionRequest }
  /** A held Slack post, which this composer draws no dock for. */
  | { kind: 'slack_draft'; request: SlackDraft };

const OPTION_KINDS: PermissionOption['kind'][] = ['allow', 'deny', 'edit', 'notes'];
const PHASES: TakePhase[] = ['recording', 'transcribing'];

function record(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === 'object' ? (value as Record<string, unknown>) : {};
}

function text(value: unknown): string | null {
  return typeof value === 'string' ? value : null;
}

function line(value: unknown): string | null {
  const held = text(value);
  return held !== null && held.trim() !== '' ? held : null;
}

function number(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

/** One of `known`, or the least-alarming member when the value is one this client is older than. */
function narrow<T extends string>(value: unknown, known: T[], fallback: T): T {
  return typeof value === 'string' && (known as string[]).includes(value) ? (value as T) : fallback;
}

/** The composer's state as the record carries it. */
export function composerFrom(value: unknown): ComposerState {
  const held = record(value);
  return {
    take: takeFrom(held['take']),
    notice: noticeFrom(held['notice']),
    compacting: held['compacting'] === true,
    signIn: signInFrom(held['sign_in']),
    bind: bindFrom(held['bind']),
    mode: modeFrom(held['mode']),
  };
}

function takeFrom(value: unknown): Take | null {
  if (value === null || value === undefined) return null;
  const held = record(value);
  const progress = Array.isArray(held['progress']) ? held['progress'] : [];
  return {
    phase: narrow(held['phase'], PHASES, 'recording'),
    levels: Array.isArray(held['levels'])
      ? held['levels'].filter((level): level is number => typeof level === 'number')
      : [],
    peakDb: number(held['peak_db']) ?? 0,
    progress: { done: number(progress[0]) ?? 0, total: number(progress[1]) },
    elapsedMs: number(held['elapsed_ms']) ?? 0,
  };
}

function noticeFrom(value: unknown): Notice | null {
  if (value === null || value === undefined) return null;
  const held = record(value);
  const text_ = text(held['text']) ?? '';
  if (held['kind'] === 'landed') {
    return { kind: 'landed', text: text_, truncated: held['truncated'] === true };
  }
  return { kind: 'line', tone: text(held['tone']) ?? 'q', text: text_ };
}

function signInFrom(value: unknown): SignIn | null {
  if (value === null || value === undefined) return null;
  const held = record(value);
  return {
    methodName: text(held['method_name']) ?? '',
    methodDescription: text(held['method_description']) ?? '',
  };
}

/** The prompt the seat is parked on, or `null` when nothing waits. */
export function askFrom(value: unknown): Ask | null {
  if (value === null || value === undefined) return null;
  const held = record(value);
  const request = held['request'];
  switch (held['kind']) {
    case 'permission':
      return permissionFrom(request);
    case 'question':
      return questionFrom(request);
    case 'slack_draft':
      return slackFrom(request);
    default:
      return null;
  }
}

function permissionFrom(value: unknown): Ask | null {
  const held = record(value);
  const call = record(held['tool_call']);
  const display = record(held['display']);
  const options = Array.isArray(held['options']) ? held['options'] : [];
  return {
    kind: 'permission',
    request: {
      toolId: text(call['tool_call_id']) ?? '',
      title: line(display['display_name']) ?? line(display['title']) ?? text(call['title']) ?? '',
      subject: subjectOf(call['raw_input']),
      reason: line(display['decision_reason']),
      description: line(display['description']),
      options: options.map((option) => {
        const row = record(option);
        return {
          optionId: text(row['option_id']) ?? '',
          name: text(row['name']) ?? '',
          kind: narrow(row['kind'], OPTION_KINDS, 'notes'),
          action: record(row['action']),
        };
      }),
    },
  };
}

function questionFrom(value: unknown): Ask | null {
  const held = record(value);
  const call = record(held['tool_call']);
  const prompt = record(held['prompt']);
  const options = Array.isArray(prompt['options']) ? prompt['options'] : [];
  return {
    kind: 'question',
    request: {
      toolId: text(call['tool_call_id']) ?? '',
      header: text(prompt['header']) ?? '',
      question: text(prompt['question']) ?? '',
      multiSelect: prompt['multi_select'] === true,
      options: options.map((option) => {
        const row = record(option);
        return {
          optionId: text(row['option_id']) ?? '',
          label: text(row['label']) ?? '',
          description: line(row['description']),
          preview: line(row['preview']),
        };
      }),
      index: number(held['question_index']) ?? 0,
      total: number(held['total_questions']) ?? 0,
    },
  };
}

/** A held post, which the dock names even though this composer holds no dock for it. */
function slackFrom(value: unknown): Ask {
  const held = record(value);
  return {
    kind: 'slack_draft',
    request: {
      id: text(held['id']) ?? '',
      workspace: text(held['workspace']) ?? '',
      conversationLabel: text(held['conversation_label']) ?? '',
      threadTs: text(held['thread_ts']),
      text: text(held['text']) ?? '',
    },
  };
}

/**
 * What a call is about, read from the field the CLI fills for the tool it named.
 *
 * The raw input is the fallback rather than nothing: Grep and Glob carry a
 * `pattern`, NotebookEdit a `notebook_path` and Task a prompt, so a list of
 * three keys leaves several tools drawing no subject at all.
 */
function subjectOf(raw: unknown): string {
  const input = record(raw);
  for (const key of ['command', 'file_path', 'url']) {
    const value = text(input[key]);
    if (value !== null) return value;
  }
  return JSON.stringify(input) ?? '';
}

/** One slash command the CLI advertised. */
export interface Advisory {
  name: string;
  description: string;
}

/** One subagent type the CLI catalogue names. */
export interface AgentType {
  name: string;
  description: string;
}

/**
 * The commands the CLI last advertised for this seat.
 *
 * The wire carries the names bare and adds the leading slash here, matching
 * `ViewSurface::slash_commands`: a row draws the name as it is typed.
 */
export function advisoriesFrom(value: unknown): Advisory[] {
  return list(value).map((entry) => {
    const row = record(entry);
    return { name: text(row['name']) ?? '', description: text(row['description']) ?? '' };
  });
}

/** The subagent types the CLI advertised, which the `&` list draws. */
export function agentTypesFrom(value: unknown): AgentType[] {
  return list(value).map((entry) => {
    const row = record(entry);
    return { name: text(row['name']) ?? '', description: text(row['description']) ?? '' };
  });
}

/** One file the seat's index holds, as the `@` list ranks it. */
export interface FileEntry {
  relPath: string;
  relPathLower: string;
  basenameLower: string;
  depth: number;
}

/** The seat's file index, keyed by path relative to the session's own root. */
export function filesFrom(value: unknown): FileEntry[] {
  const entries = record(record(value)['entries']);
  return Object.values(entries).map((entry) => {
    const row = record(entry);
    const relPath = text(row['rel_path']) ?? '';
    return {
      relPath,
      relPathLower: text(row['rel_path_lower']) ?? relPath.toLowerCase(),
      basenameLower: text(row['basename_lower']) ?? relPath.toLowerCase(),
      depth: number(row['depth']) ?? 0,
    };
  });
}

function list(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

/** Nothing a composer draws, for a seat whose page is still reading. */
export const NO_COMPOSER: ComposerState = {
  take: null,
  notice: null,
  compacting: false,
  signIn: null,
  bind: 'right_cmd',
  mode: 'auto',
};

/** The slot a command is addressed to, which every command the composer sends carries. */
export type { SessionSlot };
