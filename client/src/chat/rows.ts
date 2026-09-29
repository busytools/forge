/**
 * One turn's messages, as the rows the chat draws.
 *
 * **This is the floor of the fold, and it is deliberately not the fold yet.**
 * It walks a turn's messages in order and draws each block as itself - prose
 * as prose, a call as a call - with no grouping of consecutive calls, no
 * family lanes and no roll-up. Those are the fold's, and they land on top of
 * this rather than replacing it: what a call row IS does not change when a run
 * of them becomes one group.
 *
 * A tool's result arrives in a USER frame, because that is the shape the wire
 * uses for it. It is not something the reader said, and drawing it as one puts
 * a command's output in a bubble attributed to the person who ran it.
 */

import type { Turn } from './conversation';
import { headline, shortPath, stripEscapes, toolName } from './text';

/** Where a call got to. The wire's own five, as `ToolCallStatus` writes them. */
export type CallStatus = 'pending' | 'in_progress' | 'completed' | 'failed' | 'killed';

/** What a call's row opens on. */
export type CallBody =
  | { kind: 'text'; text: string }
  | { kind: 'diff'; path: string; old: string; new: string }
  | { kind: 'image'; mime: string | null; uri: string | null };

/** One call, as its own row. */
export interface CallRow {
  kind: 'call';
  key: string;
  /** The wire's id for the call, which is what its result names. */
  id: string;
  /** The CLI's own tool name. */
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

/** One block of prose, the assistant's or the reader's own. */
export interface ProseRow {
  kind: 'prose';
  key: string;
  text: string;
  /** Whether the reader wrote it, which is the only difference the page draws. */
  mine: boolean;
}

export type Row = CallRow | ProseRow;

/** The tools whose call carries its own diff in its input. */
const MUTATIONS = new Set(['Edit', 'Write', 'MultiEdit', 'NotebookEdit']);

/**
 * Whether a call's body is drawn without being asked for.
 *
 * A mutation's diff is what a reader came for, and the design draws it already
 * open. Every other call waits to be asked, which is the difference from the
 * terminal: it expands everything at once and the page opens one at a time.
 */
export function opensByDefault(name: string): boolean {
  return MUTATIONS.has(name);
}

/** A field of a JSON object, when it is a string. */
function text(value: unknown, key: string): string | null {
  const field = (value as Record<string, unknown> | null)?.[key];
  return typeof field === 'string' ? field : null;
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
}

function blocks(content: unknown): Block[] {
  return Array.isArray(content) ? (content as Block[]) : [];
}

/**
 * What a call's result recorded, from the shapes a tool result arrives in.
 *
 * The content is a string for most tools and a block array for a few, and both
 * are read here: a result drawn as nothing is a row that expands to an empty
 * box.
 */
function bodyOf(content: unknown): CallBody[] {
  if (typeof content === 'string') {
    const text = stripEscapes(content);
    return text.trim() === '' ? [] : [{ kind: 'text', text }];
  }
  const out: CallBody[] = [];
  for (const block of blocks(content)) {
    if (block.type === 'text' && typeof block.text === 'string') {
      const text = stripEscapes(block.text);
      if (text.trim() !== '') out.push({ kind: 'text', text });
    }
    if (block.type === 'image') {
      out.push({
        kind: 'image',
        mime: text(block, 'media_type') ?? null,
        uri: text(block, 'url') ?? null,
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
function diffOf(name: string, input: unknown): CallBody[] {
  if (!MUTATIONS.has(name)) return [];
  const path = text(input, 'file_path') ?? text(input, 'notebook_path');
  if (path === null) return [];

  if (name === 'Write') return only({ path, old: '', new: text(input, 'content') ?? '' });
  if (name === 'NotebookEdit') {
    return only({ path, old: '', new: text(input, 'new_source') ?? '' });
  }
  if (name === 'MultiEdit') {
    const edits = (input as { edits?: unknown } | null)?.edits;
    if (!Array.isArray(edits)) return [];
    return edits.flatMap((edit) =>
      only({
        path,
        old: text(edit, 'old_string') ?? '',
        new: text(edit, 'new_string') ?? '',
      }),
    );
  }
  return only({
    path,
    old: text(input, 'old_string') ?? '',
    new: text(input, 'new_string') ?? '',
  });
}

/** One hunk, or none when it says nothing: an empty old side and an empty new one is no change. */
function only(hunk: { path: string; old: string; new: string }): CallBody[] {
  if (hunk.old.trim() === '' && hunk.new.trim() === '') return [];
  return [{ kind: 'diff', ...hunk }];
}

/**
 * The rows one turn draws, oldest first.
 *
 * `cwd` is the session's working tree, which the titles are shortened against;
 * `null` leaves them as the wire wrote them.
 */
export function rowsOf(turn: Turn, cwd: string | null): Row[] {
  const rows: Row[] = [];
  /** The calls so far, by the id their result will name. */
  const calls = new Map<string, CallRow>();
  let proseCount = 0;

  for (const [at, message] of turn.messages.entries()) {
    const envelope = (message as { message?: { content?: unknown } } | null)?.message;
    const kind = (message as { type?: unknown } | null)?.type;
    const uuid = text(message, 'uuid') ?? `m${at}`;

    for (const block of blocks(envelope?.content)) {
      if (block.type === 'text' && typeof block.text === 'string') {
        const value = block.text;
        // A frame carrying nothing but tool results has no text worth a row,
        // and a tool result's own content is read by the call it answers.
        if (kind === 'user' && value.trim() === '') continue;
        proseCount += 1;
        rows.push({
          kind: 'prose',
          key: `${uuid}-t${proseCount}`,
          text: value,
          mine: kind === 'user',
        });
        continue;
      }

      if (block.type === 'tool_use' || block.type === 'server_tool_use') {
        const id = typeof block.id === 'string' ? block.id : `${uuid}-c${rows.length}`;
        const name = typeof block.name === 'string' ? block.name : 'tool';
        const row: CallRow = {
          kind: 'call',
          key: `call-${id}`,
          id,
          name,
          title: titleOf(name, block.input, cwd),
          command: text(block.input, 'command')?.trim() ?? null,
          status: 'pending',
          body: diffOf(name, block.input),
        };
        calls.set(id, row);
        rows.push(row);
        continue;
      }

      if (block.type === 'tool_result') {
        const id = typeof block.tool_use_id === 'string' ? block.tool_use_id : '';
        const call = calls.get(id);
        if (call === undefined) continue;
        call.status = block.is_error === true ? 'failed' : 'completed';
        call.body = [...call.body, ...bodyOf(block.content)];
      }
    }
  }

  return rows;
}

/** What names a call on its row: its input where that says something, its tool otherwise. */
function titleOf(name: string, input: unknown, cwd: string | null): string {
  const said = headline(name, input);
  return said === name ? toolName(name) : shortPath(said, cwd);
}
