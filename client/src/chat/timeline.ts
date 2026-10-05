/**
 * A dispatch's own frames, read into the timeline its row draws.
 *
 * **The card says how the instance is doing; the frames say what it did.**
 * The record's card carries the liveness, the usage and the last four calls;
 * the turn's own messages carry every call in order, each with its input, its
 * result and its clock. They are already in hand: a dispatched agent's frames
 * ride the turn they ran in, under the parent id that names the dispatch.
 *
 * **Each call is built as the session's own tool leaf.** The row draws them
 * through the same component the session draws its calls with - same glyph,
 * same title, same body - because a second rendering of the same facts is a
 * second thing to keep in step.
 *
 * Read lazily, only for a row that is open: a shut row pays nothing.
 */

import { blocksOf, leafOf, type Block, type ToolLeaf } from './leaves';

/** One line of the instance's own work. */
export type SubLine =
  | { kind: 'call'; leaf: ToolLeaf }
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
  /** The lines: the instance's calls (as the session's own tool rows) and the
   * prose between them, in order. */
  lines: SubLine[];
  /** The calls the CLI's heartbeat has beaten for, by call id. */
  beats: Set<string>;
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
}

function obj(value: unknown): Record<string, unknown> {
  return typeof value === 'object' && value !== null ? (value as Record<string, unknown>) : {};
}

function str(value: unknown): string | null {
  return typeof value === 'string' && value !== '' ? value : null;
}

/** One call the instance made, before its result has been folded in. */
interface CallDraft {
  kind: 'call';
  id: string;
  name: string;
  input: unknown;
  result: Block | undefined;
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
  const lines: (CallDraft | { kind: 'prose'; text: string })[] = [];
  const at = new Map<string, CallDraft>();
  const beats = new Set<string>();
  let brief: string | null = null;
  let model: string | null = null;
  let isolation: string | null = null;
  let taskId: string | null = null;
  let depth: number | null = null;
  let taskType: string | null = null;
  let outputFile: string | null = null;

  for (const raw of messages) {
    const frame = obj(raw) as Frame;
    const parent = str(frame.parent_tool_use_id);

    // The heartbeat names the call it beats for, so it lands on that line.
    if (frame.type === 'tool_progress') {
      if (parent !== null) beats.add(parent);
      continue;
    }

    if (parent === dispatchId) {
      for (const block of blocksOf(frame.message?.content)) {
        if (frame.type === 'assistant' && block.type === 'tool_use') {
          const id = str(block.id);
          const name = str(block.name);
          if (id === null || name === null) continue;
          const draft: CallDraft = { kind: 'call', id, name, input: block.input, result: undefined };
          lines.push(draft);
          at.set(id, draft);
        } else if (frame.type === 'assistant' && block.type === 'text') {
          const text = String(block.text ?? '').trim();
          if (text !== '') lines.push({ kind: 'prose', text });
        } else if (frame.type === 'user' && block.type === 'tool_result') {
          const owner = at.get(str(block.tool_use_id) ?? '');
          if (owner === undefined) continue;
          owner.result = block;
          beats.delete(owner.id);
        }
      }
      continue;
    }

    if (parent !== null) continue;

    // The dispatch itself: its input carries the brief, a named model and an
    // isolation request, none of which repeat anywhere else.
    if (frame.type === 'assistant') {
      for (const block of blocksOf(frame.message?.content)) {
        if (block.type !== 'tool_use' || str(block.id) !== dispatchId) continue;
        const input = obj(block.input);
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
        outputFile = str(frame.output_file) ?? outputFile;
      }
    }
  }

  return {
    brief,
    model,
    isolation,
    taskId,
    depth,
    taskType,
    outputFile,
    // Each call becomes the session's own tool leaf, built by the same
    // builder the conversation fold uses.
    lines: lines.map((line) =>
      line.kind === 'prose'
        ? line
        : { kind: 'call', leaf: leafOf(line.id, line.name, line.input, line.result) },
    ),
    beats,
  };
}
