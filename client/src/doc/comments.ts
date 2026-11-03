/**
 * The viewer's comment path, pure: where the document's lines are, what a
 * draft carries, and the two spellings the store takes.
 *
 * **A thread is placed by its first captured line's content**, the way a
 * code thread is placed by its anchored line: the server re-anchors each
 * document thread against the document it is read with, and this file
 * turns that landing into where the viewer draws it. A landing that moved
 * draws at its new line; one with no landing draws at its captured line,
 * marked stale; and one whose captured line is past the document has
 * nowhere to draw at all, with the reason said in words.
 *
 * **Drafts outlive the viewer.** They are held per scope, module-wide, so
 * a second hand-off that raises another document does not take unsent
 * comments on the first with it.
 */

import { renderProseBlocks } from '../chat/prose';
import { anchorHash } from '../review/write';
import type { ReviewThreadRecord } from '../session/wire';
import type { ThreadPayload } from '../review/write';

/** One line of the document, numbered from one. */
interface DocLine {
  number: number;
  text: string;
}

/** One comment target of the viewer: a rendered markdown block, or a raw
 *  line of a plain-text document. */
export interface DocRow {
  /** 1-based first source line - the line a comment anchors on. */
  line: number;
  /** 1-based last source line, inclusive: the row's own span. */
  end: number;
  /** The row's source lines, as a comment's captured context, capped. */
  context: string[];
  /** The rendered HTML for a markdown row; null draws the raw line. */
  html: string | null;
  /** The raw text of a text row, drawn escaped; '' on markdown rows. */
  text: string;
}

/** How many of a row's source lines a comment captures. A block can be long
 *  and the captured lines are read again in the outdated section, so they
 *  are capped - the row's FIRST line is always among them, because that is
 *  the line the anchor names. */
const CONTEXT_CAP = 12;

/**
 * The document as its comment targets.
 *
 * **A markdown document's rows are its rendered blocks**, because that is
 * what a reader points at: each block carries the source lines it came from,
 * the blank lines it owns, and a comment can anchor to its first line or to
 * any line its gutter names. A plain-text document has no blocks, so its
 * rows are its own lines.
 */
export function rowsOf(text: string, kind: 'markdown' | 'text'): DocRow[] {
  const lines = linesOf(text);
  if (kind === 'text') {
    return lines.map((line) => ({
      line: line.number,
      end: line.number,
      context: [line.text],
      html: null,
      text: line.text,
    }));
  }
  const blocks = renderProseBlocks(text);
  return blocks.map((block, at) => ({
    line: block.line,
    // **Every source line is a target, so a block owns the blank lines
    // between it and the next one.** Without them the gutter skips the gaps,
    // and a document with a blank line between every paragraph carries no
    // target for half its lines. The rendered span stays in `context`.
    end: blocks[at + 1] === undefined ? lines.length : Math.max(block.end, blocks[at + 1].line - 1),
    context: lines
      .slice(block.line - 1, Math.min(block.end, block.line - 1 + CONTEXT_CAP))
      .map((held) => held.text),
    html: block.html,
    text: '',
  }));
}

/** The scope the store keys a document's threads under - the crate's
 *  `doc_scope_ref`, spelled here for the client half. */
export function docScope(path: string): string {
  return `doc:${path}`;
}

/** The document's lines, in order, blank ones kept: the numbers a comment
 *  anchors by are the raw file's, not the rendered blocks'. A file's final
 *  newline ends the last line rather than opening an empty one. */
export function linesOf(text: string): DocLine[] {
  const parts = text.split('\n');
  if (parts.length > 1 && parts[parts.length - 1] === '') parts.pop();
  return parts.map((line, at) => ({ number: at + 1, text: line }));
}

/**
 * A line comment's captured lines, the anchored line FIRST.
 *
 * The first captured line is the thread's identity across a revision - the
 * server re-finds it by that line's content - so it is the line the reader
 * pressed, never a neighbour. The lines after it are the reader's context
 * when the comment is read again.
 */
export function lineContext(lines: DocLine[], line: number): string[] {
  const at = line - 1;
  const captured: string[] = [];
  const first = lines[at];
  if (first === undefined) return captured;
  captured.push(first.text);
  for (let next = at + 1; next < lines.length && captured.length < CONTEXT_CAP; next += 1) {
    captured.push(lines[next]?.text ?? '');
  }
  return captured;
}

/** One thread as the viewer draws it: where, and whether it is stale. */
export interface DocThreadPlacement {
  thread: ReviewThreadRecord;
  /** The line it draws under now, or null when it has nowhere to draw. */
  line: number | null;
  /** The line it was written at, when the landing moved it. */
  movedFrom: number | null;
  /** Its captured line is gone or ambiguous in this version. */
  stale: boolean;
  /** Why it has nowhere to draw, when `line` is null. */
  why: string | null;
}

/**
 * Every thread of the document, where this version lets it draw.
 *
 * **The server's landing decides, never the rendered row.** A `placed`
 * landing draws at the line it names; anything else is stale at its
 * captured line, because a thread the revision could not re-find is still
 * the reader's, and collecting it at the bottom would hide what it was
 * about. Only a captured line the document no longer reaches has nowhere
 * to draw, and that one carries the sentence saying so.
 */
export function placeThreads(
  threads: ReviewThreadRecord[],
  lineCount: number,
): DocThreadPlacement[] {
  return threads.map((thread) => {
    const resolution = thread.resolution;
    if (resolution?.state === 'placed') {
      return {
        thread,
        line: resolution.line,
        movedFrom: resolution.moved_from,
        stale: false,
        why: null,
      };
    }
    const captured = thread.anchor.line;
    if (captured >= 1 && captured <= lineCount) {
      return { thread, line: captured, movedFrom: null, stale: true, why: null };
    }
    return {
      thread,
      line: null,
      movedFrom: null,
      stale: true,
      why: `line ${captured} is past this version's ${lineCount} lines`,
    };
  });
}

/** One unsent comment, as the viewer holds it before the finish files it. */
export interface DocDraft {
  id: string;
  line: number;
  text: string;
  /** The lines around the anchor, captured at compose time. */
  context: string[];
}

/**
 * The drafts, per scope. **Module-wide on purpose**: the route's state is
 * rebuilt when a second hand-off moves the viewer to another document, and
 * a map here is what keeps the first document's unsent comments until
 * their finish files them.
 */
const drafts = new Map<string, DocDraft[]>();

export function draftsFor(scope: string): DocDraft[] {
  return drafts.get(scope) ?? [];
}

export function addDraft(scope: string, draft: DocDraft): void {
  drafts.set(scope, [...draftsFor(scope), draft]);
}

/** Replace one draft's words; the id - and so its stored thread - stays. */
export function editDraftText(scope: string, id: string, text: string): DocDraft | null {
  const at = draftsFor(scope);
  const found = at.find((draft) => draft.id === id);
  if (found === undefined) return null;
  const next = { ...found, text };
  drafts.set(
    scope,
    at.map((draft) => (draft.id === id ? next : draft)),
  );
  return next;
}

/** Clear the drafts a finish filed; the ids are the only ones that went. */
export function clearDrafts(scope: string, ids: string[]): void {
  const gone = new Set(ids);
  drafts.set(
    scope,
    draftsFor(scope).filter((draft) => !gone.has(draft.id)),
  );
}

/** One draft as the store's own thread shape, anchored to the document.
 *
 *  The hash is the FIRST captured line's, the way a code anchor hashes its
 *  anchored line: that line is what a revision re-finds the thread by. */
export function docThread(draft: DocDraft, path: string): ThreadPayload {
  return {
    id: draft.id,
    anchor: {
      path,
      side: 'New',
      line: draft.line,
      content_hash: anchorHash(draft.context[0] ?? '').toString(),
      context: draft.context,
      base_ref: docScope(path),
    },
    comments: [{ author: 'User', text: draft.text, at: '', review_id: null }],
    status: 'Open',
    created_at: '',
    updated_at: '',
    commit: null,
  };
}
