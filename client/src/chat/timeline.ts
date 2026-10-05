/**
 * A dispatch's own frames, read into the timeline its row draws.
 *
 * **The card says how the instance is doing; the frames say what it did.** The
 * record's card carries the liveness, the usage and the last four calls; the
 * turn's own messages carry every call in order, each with its input, its
 * result and its clock - so the row's expansion reads them here rather than
 * the client's conversation fold putting them on screen. They are already in
 * hand: a dispatched agent's frames ride the turn they ran in, under the
 * parent id that names the dispatch.
 *
 * Read lazily, only for a row that is open: a shut row pays nothing.
 */

import { titleOf } from './leaves';

/** One line of the instance's own work. */
export type SubLine =
  | {
      kind: 'call';
      id: string;
      name: string;
      title: string;
      /** What the call was given, on one line: the command it ran, the file it read. */
      input: string;
      /** What it came back with, when it has come back. */
      output: string | null;
      status: 'in_progress' | 'completed' | 'failed';
      /** Whether the CLI's heartbeat for this call has been seen. */
      beat: boolean;
    }
  | { kind: 'prose'; text: string };

/** Everything the row's expansion reads off a dispatch's frames. */
export interface DispatchFrames {
  /** The brief the dispatch carried, which is the prompt it was given. */
  brief: string | null;
  /** The model the dispatch named, when it named one. */
  model: string | null;
  /** The isolation the dispatch asked for (a worktree), when it asked. */
  isolation: string | null;
  /** The CLI's own handle for the task, off the roster's first frame. */
  taskId: string | null;
  /** How deep the task nested, off the roster. */
  depth: number | null;
  /** The CLI's word for what kind of task it is (`local_agent`, `local_bash`). */
  taskType: string | null;
  /** Where the CLI writes the instance's own transcript, off its ending. */
  outputFile: string | null;
  /** The lines: the instance's calls and the prose between them, in order. */
  lines: SubLine[];
  /** The CLI's own outcome line, from the task notification when one arrived. */
  summary: string | null;
}

/** The frame shapes this reader touches, as loosely as the wire is held. */
interface Frame {
  type?: unknown;
  subtype?: unknown;
  parent_tool_use_id?: unknown;
  tool_use_id?: unknown;
  task_id?: unknown;
  spawn_depth?: unknown;
  task_type?: unknown;
  output_file?: unknown;
  message?: { content?: unknown };
  timestamp?: unknown;
  summary?: unknown;
}

function obj(value: unknown): Record<string, unknown> {
  return typeof value === 'object' && value !== null ? (value as Record<string, unknown>) : {};
}

function str(value: unknown): string | null {
  return typeof value === 'string' && value !== '' ? value : null;
}

function blocks(content: unknown): Record<string, unknown>[] {
  if (typeof content === 'string') return [{ type: 'text', text: content }];
  return Array.isArray(content) ? content.map((one) => obj(one)) : [];
}

/** A result's own text, which is a string or a list of text blocks. */
function textOf(content: unknown): string {
  if (typeof content === 'string') return content;
  return blocks(content)
    .filter((block) => block['type'] === 'text')
    .map((block) => String(block['text'] ?? ''))
    .join('\n');
}

/** What a call was given, on the one line the timeline draws it as. */
function inputLine(name: string, input: unknown): string {
  const held = obj(input);
  for (const key of ['command', 'file_path', 'pattern', 'path', 'url', 'prompt', 'query']) {
    const said = str(held[key]);
    if (said !== null) return said;
  }
  const all = JSON.stringify(input ?? null);
  return all === undefined ? name : all;
}

/**
 * The dispatch's own frames, read into what the row draws.
 *
 * `messages` is the turn's messages as the wire carried them - the fold hands
 * the row's timeline these rather than the units, because the units are the
 * conversation's and a dispatched agent's frames are not in them.
 */
export function dispatchFrames(
  messages: readonly unknown[],
  dispatchId: string,
): DispatchFrames {
  const lines: SubLine[] = [];
  const at = new Map<string, SubLine & { kind: 'call' }>();
  let brief: string | null = null;
  let model: string | null = null;
  let isolation: string | null = null;
  let taskId: string | null = null;
  let depth: number | null = null;
  let taskType: string | null = null;
  let outputFile: string | null = null;
  let summary: string | null = null;

  for (const raw of messages) {
    const frame = obj(raw) as Frame;
    const parent = str(frame.parent_tool_use_id);

    // The heartbeat names the call it beats for, so it lands on that line.
    if (frame.type === 'tool_progress') {
      const owner = at.get(parent ?? '');
      if (owner !== undefined) owner.beat = true;
      continue;
    }

    if (parent === dispatchId) {
      for (const block of blocks(frame.message?.content)) {
        if (frame.type === 'assistant' && block['type'] === 'tool_use') {
          const id = str(block['id']);
          const name = str(block['name']);
          if (id === null || name === null) continue;
          const line: SubLine & { kind: 'call' } = {
            kind: 'call',
            id,
            name,
            title: titleOf(name, block['input']),
            input: inputLine(name, block['input']),
            output: null,
            status: 'in_progress',
            beat: false,
          };
          lines.push(line);
          at.set(id, line);
        } else if (frame.type === 'assistant' && block['type'] === 'text') {
          const text = String(block['text'] ?? '').trim();
          if (text !== '') lines.push({ kind: 'prose', text });
        } else if (frame.type === 'user' && block['type'] === 'tool_result') {
          const owner = at.get(str(block['tool_use_id']) ?? '');
          if (owner === undefined) continue;
          owner.output = textOf(block['content']);
          owner.status = block['is_error'] === true ? 'failed' : 'completed';
          owner.beat = false;
        }
      }
      continue;
    }

    if (parent !== null) continue;

    // The dispatch itself: its input carries the brief, a named model and an
    // isolation request, none of which repeat anywhere else.
    if (frame.type === 'assistant') {
      for (const block of blocks(frame.message?.content)) {
        if (block['type'] !== 'tool_use' || str(block['id']) !== dispatchId) continue;
        const input = obj(block['input']);
        brief = str(input['prompt']);
        model = str(input['model']);
        isolation = str(input['isolation']);
      }
    }
    // The roster's own facts about the task the dispatch opened: its handle,
    // its depth, its kind, and where its transcript is written.
    if (frame.type === 'system' && str(frame.tool_use_id) === dispatchId) {
      if (frame.subtype === 'task_started') {
        taskId = str(frame.task_id) ?? taskId;
        depth = typeof frame.spawn_depth === 'number' ? frame.spawn_depth : depth;
        taskType = str(frame.task_type) ?? taskType;
      }
      if (frame.subtype === 'task_notification' || frame.subtype === 'task_updated') {
        summary = str(frame.summary) ?? summary;
        outputFile = str(frame.output_file) ?? outputFile;
      }
    }
  }

  return { brief, model, isolation, taskId, depth, taskType, outputFile, lines, summary };
}
