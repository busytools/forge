/**
 * One call, as the row it draws.
 *
 * **A call's result arrives in a USER frame**, because that is the shape the
 * wire uses for it. It is not something the reader said, and drawing it as one
 * puts a command's output in a bubble attributed to the person who ran it.
 *
 * **What a call's body is comes from two places**, and both have to be read:
 * a mutation's diff is in the call's own INPUT, because the wire sends it with
 * the call, and everything else is in the result that answers it.
 */

import { languageOf } from './code';
import { isEdit, type CallStatus, type KindRow, rowOf } from './families';
import { headline, stripEscapes, toolName } from './text';

/** What a call's row opens on. */
export type CallBody =
  | { kind: 'text'; text: string }
  | { kind: 'diff'; old: string; new: string }
  | { kind: 'image'; mime: string | null; uri: string | null };

/**
 * What the wire reports about a call it runs as a TASK, backgrounded or not.
 *
 * The launch result of a backgrounded call says only that the command started,
 * so a fold that settles the call from its result draws a running command as a
 * finished one. The task frames are what say otherwise: `task_started` opens
 * the call's own clock and says whether it outlives its turn, `task_updated`
 * carries the ending, and `task_notification` carries what the harness said.
 */
export interface BackgroundTask {
  status: CallStatus;
  /**
   * Whether the wire said this task outlives its turn.
   *
   * **The note is only for that case.** A foreground call's own result already
   * carries what it came back with, and a dispatched agent's report is the
   * call's own body - drawing the harness's summary under either one repeats
   * what the row already says, once in the row's title and once in the line.
   */
  backgrounded: boolean;
  /**
   * What the harness said when the task ended, when it said anything.
   *
   * The tone is the status word's own reading - green for a task that
   * finished, red for one that failed or was killed - and `null` for a word
   * this page does not know, which is not a failure and is not a success.
   */
  note: { text: string; tone: 'sum' | 'fail' | null } | null;
}

/** One call, as its own row. */
export interface ToolLeaf {
  /** The `tool_use` id the wire gave it, which is what its result names. */
  id: string;
  /** The class this call belongs to, which a row picks its glyph from. */
  row: KindRow;
  /** The CLI's own name for the tool. */
  name: string;
  /** What the row draws as the call's name. */
  title: string;
  /**
   * The command the call ran, when it ran one.
   *
   * Separate from the title because a call given a description is named by
   * that instead, and the command it actually ran would then appear nowhere:
   * the body leads with it, so the whole of it stays readable even where the
   * row's own title is capped.
   */
  command: string | null;
  status: CallStatus;
  /**
   * What the harness said when a BACKGROUNDED call ended: drawn as the last
   * line of the box the call's result drew, and `null` for every other call.
   */
  note: { text: string; tone: 'sum' | 'fail' | null } | null;
  /** What the row opens on. Empty for a call that has not come back yet. */
  body: CallBody[];
}

/** Whether a call's body is drawn without being asked for. */
export function opensByDefault(name: string): boolean {
  return isEdit(name);
}

/** Whether a call's body is a source file the page colours. */
export function readsSource(name: string): boolean {
  return name === 'Read';
}

/** A field of a JSON object, when it is a string. */
function field(value: unknown, key: string): string | null {
  const held = (value as Record<string, unknown> | null)?.[key];
  return typeof held === 'string' ? held : null;
}

/** A block, as one of the shapes the wire uses. */
export interface Block {
  type?: unknown;
  text?: unknown;
  id?: unknown;
  name?: unknown;
  input?: unknown;
  tool_use_id?: unknown;
  content?: unknown;
  is_error?: unknown;
  media_type?: unknown;
  url?: unknown;
  source?: unknown;
  prompt?: unknown;
  commandMode?: unknown;
  thinking?: unknown;
}

/** The blocks of a frame's content, for a frame that carries any. */
export function blocksOf(content: unknown): Block[] {
  return Array.isArray(content) ? (content as Block[]) : [];
}

/**
 * What a call's result recorded, from the shapes a tool result arrives in.
 *
 * The content is a string for most tools and a block array for a few, and both
 * are read here: a result drawn as nothing is a row that expands to an empty
 * box.
 */
export function bodyOf(content: unknown): CallBody[] {
  if (typeof content === 'string') {
    const text = stripEscapes(content);
    return text.trim() === '' ? [] : [{ kind: 'text', text }];
  }
  const out: CallBody[] = [];
  for (const block of blocksOf(content)) {
    if (block.type === 'text' && typeof block.text === 'string') {
      const text = stripEscapes(block.text);
      if (text.trim() !== '') out.push({ kind: 'text', text });
    }
    if (block.type === 'image') {
      // The wire nests both under `source`, which is the shape a user turn's
      // own attachment carries: read off the block they are always absent, and
      // a tool's image draws with no mime at all.
      out.push({
        kind: 'image',
        mime: field(block.source, 'media_type'),
        uri: field(block.source, 'url'),
      });
    }
  }
  return out;
}

/**
 * The diff a mutation carries in its own input, which is where the wire puts
 * it.
 *
 * **The four mutations do not share one input shape, and each writes its
 * sides differently.** `Write` carries the whole FILE rather than a change to
 * it, so its content is the ADDED side and nothing was removed: read the
 * other way round it draws a file the session just wrote as a file it
 * deleted, in a card that opens without being clicked. `MultiEdit` carries a
 * list of edits, one hunk each. `NotebookEdit` carries a cell's new source
 * and no old side at all.
 *
 * A mutation whose input this page cannot read draws no diff rather than an
 * empty card, which is what an unread shape would otherwise leave open on
 * screen.
 */
export function diffsOf(name: string, input: unknown): CallBody[] {
  if (!isEdit(name)) return [];
  const path = field(input, 'file_path') ?? field(input, 'notebook_path');
  if (path === null) return [];

  if (name === 'Write') return hunk('', field(input, 'content') ?? '');
  if (name === 'NotebookEdit') return hunk('', field(input, 'new_source') ?? '');
  if (name === 'MultiEdit') {
    const edits = (input as { edits?: unknown } | null)?.edits;
    if (!Array.isArray(edits)) return [];
    return edits.flatMap((edit) =>
      hunk(field(edit, 'old_string') ?? '', field(edit, 'new_string') ?? ''),
    );
  }
  return hunk(field(input, 'old_string') ?? '', field(input, 'new_string') ?? '');
}

/**
 * One hunk, or none when it says nothing: an empty old side and an empty new
 * one is no change.
 *
 * **The path is checked above and not carried here.** The row's own title is
 * the file, whole, so a hunk naming it again drew the same path twice on one
 * row; what is left of the check is that a mutation with no path in its input
 * draws nothing at all.
 */
function hunk(old: string, added: string): CallBody[] {
  if (old.trim() === '' && added.trim() === '') return [];
  return [{ kind: 'diff', old, new: added }];
}

/**
 * What names a call on its row: its input where that says something, its tool otherwise.
 *
 * **The path as the wire gave it rather than cut against the working tree.** A
 * mutation's row draws the same file twice - its title, and the header of the
 * diff under it - so shortening one of them is the row disagreeing with itself
 * about which file it is. The terminal puts the whole of it on the row (its
 * `create_tool_call` builds `Edit /Users/.../home.css`), which is the reference
 * this follows.
 */
export function titleOf(name: string, input: unknown): string {
  const said = headline(name, input);
  return said === name ? toolName(name) : said;
}

/**
 * One call, from the wire's own fields.
 *
 * `result` is what a later frame recorded for it, when one has arrived: a call
 * that has not come back yet still carries what its own input says, because an
 * edit's diff is in the call rather than in the answer.
 *
 * `task` settles the call when the wire gave it an ending, and holds it open
 * only where the wire said the task OUTLIVES its turn: such a call's launch
 * result says only that the command started, so it has to draw as running
 * however clean that result was. A foreground task still running is settled by
 * its result and the turn's verdict, like a call with no task frame at all -
 * so an abandoned foreground call draws failed rather than running.
 *
 * `abandoned` is the turn's verdict on a call it never answered: a turn that
 * failed is a turn whose open calls never get a result, and they draw as
 * failed rather than as still running - which is what the terminal does with
 * them on the same frame.
 */
export function leafOf(
  id: string,
  name: string,
  input: unknown,
  result: Block | undefined,
  task: BackgroundTask | undefined = undefined,
  abandoned = false,
): ToolLeaf {
  const body = diffsOf(name, input);
  const settled =
    task !== undefined && (task.backgrounded || task.status !== 'in_progress')
      ? task.status
      : settledBy(result, abandoned);
  return {
    id,
    row: rowOf(name),
    name,
    title: titleOf(name, input),
    command: field(input, 'command')?.trim() || null,
    status: settled,
    note: task?.backgrounded === true ? task.note : null,
    body: drawnBody(name, body, result),
  };
}

/**
 * What a call's body draws, which for a mutation is its diff and nothing else.
 *
 * The result's own text repeats the diff's path and carries the CLI's note on
 * the file state, which is an instruction to the model rather than anything a
 * reader acts on; the terminal draws the diff and returns there for both `Edit`
 * and `Write` (`build_tool_result_fields`). Where there is no diff the text is
 * all there is, and either way the call still settles on its result.
 */
function drawnBody(name: string, body: CallBody[], result: Block | undefined): CallBody[] {
  if (result === undefined) return body;
  const answered = bodyOf(result.content);
  if (isEdit(name) && body.some((part) => part.kind === 'diff')) return body;
  return [...body, ...answered];
}

/** What settles a call with no task frame of its own: its result, or the turn that never sent one. */
function settledBy(result: Block | undefined, abandoned: boolean): CallStatus {
  if (result === undefined) return abandoned ? 'failed' : 'pending';
  return result.is_error === true ? 'failed' : 'completed';
}

/** Whether a call's body draws as a source file: a read that named one this page knows. */
export function languageFor(leaf: ToolLeaf): string | null {
  return readsSource(leaf.name) ? languageOf(leaf.title) : null;
}
