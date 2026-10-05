/**
 * The reader's anchor: the row their eye is on, and how far into it they are.
 *
 * **Measured in rows, never in pixels.** The column's layout moves under a
 * reader who is not at the foot - a row measures taller once it is drawn, a
 * stretch is corrected as frames land - and an offset that stays put then
 * reads as the page sliding under them (Ved, 2026-10-03). The terminal keeps
 * the same record for the same reason (`ChatViewport`'s message-local anchor,
 * restored across a layout remeasure); this is its client half. The row is
 * found by the key the fold gave it, which the rows draw in `data-k`, so a
 * row that MOVES takes the reader with it.
 */
export interface Anchor {
  /** The fold's own key for the row, which the row draws in `data-k`. */
  key: string;
  /** How far into the row the viewport's top sits, in whole pixels. */
  into: number;
}

/** One row as the column can see it: its key, and its box in the viewport. */
export interface RowBox {
  key: string;
  top: number;
  bottom: number;
}

/**
 * The row the viewport's top edge lands in, or null where none reaches it.
 *
 * The first row whose box passes the top edge: one straddling it counts, and one
 * entirely above does not. A row entirely below leaves `into` negative, which
 * is what it is - the reader's top edge sits above that row.
 *
 * **An iterable rather than an array, and the loop breaks at the first
 * crossing.** Measuring a box is a layout read, and the caller runs this on
 * every scroll event with the giant seat's window drawn: a generator measures
 * rows only until the edge is found, and what is below it is never asked for.
 */
export function anchorAt(rows: Iterable<RowBox>, viewportTop: number): Anchor | null {
  for (const row of rows) {
    if (row.bottom > viewportTop) {
      return { key: row.key, into: Math.round(viewportTop - row.top) };
    }
  }
  return null;
}

/**
 * Where the column has to scroll to hold the anchored row where it was.
 *
 * `top` is the row's own top within the column's content, and the answer is
 * that top PLUS the distance the reader sat into the row: the viewport's top
 * edge has to land inside the row exactly where it was, so a row the layout
 * pushed down by N scrolls the column down by N and the reader's eye stays on
 * the same words. It is the exact inverse of `anchorAt`'s capture, which is the
 * arm the tests pin. Nothing here clamps - the browser clamps a scroll to what
 * the column holds, the way it does for every other pin.
 */
export function anchoredScroll(anchor: Anchor, top: number): number {
  return Math.round(top + anchor.into);
}
