/**
 * The viewer's comment path, pure: where the document's lines are, what a
 * draft carries, and the two spellings the store takes.
 *
 * **A thread is placed by the document's own hash.** A doc-scoped anchor's
 * `content_hash` is the whole document's - the crate's FNV-1a over its
 * bytes, crossing as decimal digits - so a comment written against this
 * exact content sits at its line, and one written against another version
 * is outdated AT ONCE, with its captured lines kept. Nothing re-anchors a
 * document automatically: that is the code review's rule read the other
 * way, where a moved line is re-found by content, and a document has no
 * diff to move within.
 *
 * **Drafts outlive the viewer.** They are held per scope, module-wide, so
 * a second hand-off that raises another document does not take unsent
 * comments on the first with it.
 */

import type { ReviewThreadRecord } from '../session/wire';
import type { ThreadPayload } from '../review/write';

/** One line of the document, numbered from one. */
export interface DocLine {
  number: number;
  text: string;
}

/** How many lines are captured on each side of an anchor, the crate's own
 *  `CONTEXT_RADIUS`. */
const CONTEXT_RADIUS = 3;

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

/** Up to `radius` lines on each side of `line`, excluding it, clamped at
 *  the document's edges - the crate's `capture_context`, over the whole
 *  document rather than a hunk. */
export function contextAround(lines: DocLine[], line: number, radius = CONTEXT_RADIUS): string[] {
  const index = line - 1;
  const start = Math.max(0, index - radius);
  const end = Math.min(lines.length, index + radius + 1);
  const captured: string[] = [];
  for (let at = start; at < end; at += 1) {
    if (at === index) continue;
    const held = lines[at];
    if (held !== undefined) captured.push(held.text);
  }
  return captured;
}

/** The threads still written against this exact document. */
export function placedThreads(
  threads: ReviewThreadRecord[],
  contentHash: string,
): ReviewThreadRecord[] {
  return threads.filter((thread) => thread.anchor.content_hash === contentHash);
}

/** The threads the document changed under: drawn outdated, at once. */
export function threadsAgainstAnotherVersion(
  threads: ReviewThreadRecord[],
  contentHash: string,
): ReviewThreadRecord[] {
  return threads.filter((thread) => thread.anchor.content_hash !== contentHash);
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

/** Clear the drafts a finish filed; the ids are the only ones that went. */
export function clearDrafts(scope: string, ids: string[]): void {
  const gone = new Set(ids);
  drafts.set(
    scope,
    draftsFor(scope).filter((draft) => !gone.has(draft.id)),
  );
}

/** One draft as the store's own thread shape, anchored to the document. */
export function docThread(draft: DocDraft, path: string, contentHash: string): ThreadPayload {
  return {
    id: draft.id,
    anchor: {
      path,
      side: 'New',
      line: draft.line,
      content_hash: contentHash,
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
