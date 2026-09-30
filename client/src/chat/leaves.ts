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
import { headline, shortPath, stripEscapes, toolName } from './text';

/** What a call's row opens on. */
export type CallBody =
  | { kind: 'text'; text: string }
  | { kind: 'diff'; path: string; old: string; new: string }
  | { kind: 'image'; mime: string | null; uri: string | null };

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
interface Block {
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
  prompt?: unknown;
  commandMode?: unknown;
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
      out.push({
        kind: 'image',
        mime: typeof block.media_type === 'string' ? block.media_type : null,
        uri: typeof block.url === 'string' ? block.url : null,
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

  if (name === 'Write') return hunk(path, '', field(input, 'content') ?? '');
  if (name === 'NotebookEdit') return hunk(path, '', field(input, 'new_source') ?? '');
  if (name === 'MultiEdit') {
    const edits = (input as { edits?: unknown } | null)?.edits;
    if (!Array.isArray(edits)) return [];
    return edits.flatMap((edit) =>
      hunk(path, field(edit, 'old_string') ?? '', field(edit, 'new_string') ?? ''),
    );
  }
  return hunk(path, field(input, 'old_string') ?? '', field(input, 'new_string') ?? '');
}

/** One hunk, or none when it says nothing: an empty old side and an empty new one is no change. */
function hunk(path: string, old: string, added: string): CallBody[] {
  if (old.trim() === '' && added.trim() === '') return [];
  return [{ kind: 'diff', path, old, new: added }];
}

/** What names a call on its row: its input where that says something, its tool otherwise. */
export function titleOf(name: string, input: unknown, cwd: string | null): string {
  const said = headline(name, input);
  return said === name ? toolName(name) : shortPath(said, cwd);
}

/**
 * One call, from the wire's own fields.
 *
 * `result` is what a later frame recorded for it, when one has arrived: a call
 * that has not come back yet still carries what its own input says, because an
 * edit's diff is in the call rather than in the answer.
 */
export function leafOf(
  id: string,
  name: string,
  input: unknown,
  result: Block | undefined,
  cwd: string | null,
): ToolLeaf {
  const body = diffsOf(name, input);
  const settled =
    result === undefined ? 'pending' : result.is_error === true ? 'failed' : 'completed';
  return {
    id,
    row: rowOf(name),
    name,
    title: titleOf(name, input, cwd),
    command: field(input, 'command')?.trim() || null,
    status: settled,
    body: result === undefined ? body : [...body, ...bodyOf(result.content)],
  };
}

/** Whether a call's body draws as a source file: a read that named one this page knows. */
export function languageFor(leaf: ToolLeaf): string | null {
  return readsSource(leaf.name) ? languageOf(leaf.title) : null;
}
