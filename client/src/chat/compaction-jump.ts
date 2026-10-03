import { fold } from './units';

/**
 * The latest turn carrying a compaction, or null when none does.
 *
 * Read from the fold rather than from the page's own records, because the
 * compaction row is the fold's: a boundary is a frame inside a turn, and the
 * turn that holds the newest one is where the reader wants to be. Searched
 * from the end, so the answer is the LAST cut in the conversation - clicking
 * the header's count goes to the newest, not to a chosen one.
 */
export function latestCompaction(
  turns: readonly { messages: readonly unknown[] }[],
): number | null {
  for (let at = turns.length - 1; at >= 0; at -= 1) {
    const turn = turns[at];
    if (turn === undefined) continue;
    if (fold(turn.messages).some((unit) => unit.kind === 'compaction')) return at;
  }
  return null;
}
