/**
 * What dictation means to the review page: the box a landed take's words go
 * in, and nothing else.
 *
 * The take itself is the page's own `Dictation`, the same one the composer
 * wears - so what is left here is the review's own half of the bargain: where
 * a landing goes, and which landings are this page's to place at all.
 */

import type { Notice } from '../composer/wire';

/** Where the comment box is open, which is what a landing is addressed to. */
export interface ReviewSpot {
  path: string;
  side: 'old' | 'new';
  line: number;
}

/**
 * The words a landed take puts in the box, or `null` when there are none to
 * place.
 *
 * **A notice about a take this page never watched is not its own.** A landed
 * notice is per-seat server state that outlives its take, so a page that
 * attaches - or reloads - finds one whose words it never saw land; only a
 * take this page watched may put words in the reader's box. And one notice is
 * placed once: the record keeps it until the next take starts, and a landing
 * re-read on every repaint would write the same words over and over.
 */
export function landed(
  notice: Notice | null,
  watched: boolean,
  last: string | null,
): string | null {
  if (notice === null || notice.kind !== 'landed' || !watched) return null;
  return notice.text === last ? null : notice.text;
}
