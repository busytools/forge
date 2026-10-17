/**
 * What the files page writes and lists: the tree scope its comments are
 * filed under, the context its anchors capture, and the rail's rows.
 *
 * **The scope is the SEAT, spelled exactly as the crate spells it**
 * (`tree_scope_ref` in forge-primitives): `tree:{org}/{project}/{label}`.
 * The two implementations are pinned together by the scope's shape - a
 * mismatch would file comments under a scope nothing reads - and this is
 * the same arrangement `docScope` has with the doc scope's prefix.
 *
 * The writes themselves are the review page's own (`review/write.ts`):
 * `newThread`, `withReply`, `submitCommand`, `finishRound`, `reopenThread`.
 * One write path, two pages - a divergence there would be two stores.
 */

import type { ReviewThreadRecord } from '../session/wire';
import type { SessionSlot } from '../wire/types';

/**
 * The scope ref a seat's tree's comments are stored under. Built from the
 * slot's own three parts, the same string `tree_scope_ref` builds for the
 * server.
 */
export function treeScope(slot: SessionSlot): string {
  return `tree:${slot.org}/${slot.project}/${slot.label}`;
}

/** How many lines are captured on each side of an anchor, the crate's own
 *  `CONTEXT_RADIUS` (`review/write.ts` keeps the same constant for its
 *  diff-line capture). */
export const CONTEXT_RADIUS = 3;

/** The context a comment on `line` captures, from the file's own lines:
 *  the crate's `capture_context`, over plain strings. */
export function contextForComment(lines: string[], line: number): string[] {
  const index = line - 1;
  const start = Math.max(0, index - CONTEXT_RADIUS);
  const end = Math.min(lines.length, index + CONTEXT_RADIUS + 1);
  const captured: string[] = [];
  for (let at = start; at < end; at += 1) {
    if (at === index) continue;
    const text = lines[at];
    if (text !== undefined) captured.push(text);
  }
  return captured;
}

/** One thread as the rail lists it, with where it draws now. */
export interface RailRow {
  thread: ReviewThreadRecord;
  /** The line the landing places it on, or null when no landing was carried. */
  line: number | null;
  /** The recorded line it moved from, when the landing says it moved. */
  movedFrom: number | null;
  /** Placed nowhere: outdated by the landing's word, or no landing at all. */
  stale: boolean;
}

/**
 * The tree scope's threads as rail rows, ordered the way a walk reads: by
 * path, then by line. A thread whose landing failed to place (outdated, or
 * none carried - a vanished file, a seat without a cwd) is `stale`, which
 * the rail draws in the outdated tone beside its captured context rather
 * than dropping it.
 */
export function railRows(threads: ReviewThreadRecord[]): RailRow[] {
  return threads
    .map((thread) => {
      const resolution = thread.resolution;
      if (resolution?.state === 'placed') {
        return {
          thread,
          line: resolution.line,
          movedFrom: resolution.moved_from,
          stale: false,
        };
      }
      return { thread, line: null, movedFrom: null, stale: true };
    })
    .sort((a, b) => {
      if (a.thread.anchor.path !== b.thread.anchor.path) {
        return a.thread.anchor.path < b.thread.anchor.path ? -1 : 1;
      }
      const left = a.line ?? a.thread.anchor.line;
      const right = b.line ?? b.thread.anchor.line;
      return left - right;
    });
}

/** The threads a file row's pill counts: every thread whose anchor names it. */
export function threadsOnPath(threads: ReviewThreadRecord[], path: string): number {
  return threads.filter((thread) => thread.anchor.path === path).length;
}
