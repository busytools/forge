import type { QueuedPromptRow } from '../session/wire';

/**
 * The pile's own arithmetic, apart from the component so its edges are
 * testable without a DOM: the walk, and where the face sits.
 *
 * A row the core settles leaves `rows` while the cursor may still name it, so
 * both functions take a cursor that can be stale - the walk re-arms at the
 * newest rather than going inert, which is what a walk that trusted the
 * cursor's index did.
 */

/** Where the face sits: the cursor's row, or the newest when the walk is in the box. */
export function faceAt(cursor: string | null, rows: QueuedPromptRow[]): number {
  if (rows.length === 0) return 0;
  if (cursor === null) return rows.length - 1;
  const at = rows.findIndex((row) => row.uuid === cursor);
  return at < 0 ? rows.length - 1 : at;
}

/**
 * Where the cursor lands: up enters at the newest and stops on the first
 * queued prompt, down comes back, and one more down past the newest lands in
 * the box - which is `null`.
 */
export function walk(
  cursor: string | null,
  rows: QueuedPromptRow[],
  direction: 'up' | 'down',
): string | null {
  if (rows.length === 0) return null;
  if (cursor === null) {
    return direction === 'up' ? (rows[rows.length - 1]?.uuid ?? null) : null;
  }
  const at = rows.findIndex((row) => row.uuid === cursor);
  if (at < 0) return null;
  if (direction === 'up') return rows[at - 1]?.uuid ?? cursor;
  return at + 1 >= rows.length ? null : (rows[at + 1]?.uuid ?? null);
}
