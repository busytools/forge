import { describe, expect, it } from 'vitest';

import { anchoredScroll, anchorAt, type RowBox } from './anchor';

const row = (key: string, top: number, bottom: number): RowBox => ({ key, top, bottom });

describe("the reader's anchor", () => {
  it("lands on the row the viewport's top edge is inside", () => {
    const rows = [row('a', 0, 40), row('b', 40, 90), row('c', 90, 150)];

    // Mid-row: the straddling row, with how far into it the edge sits.
    expect(anchorAt(rows, 60), 'inside b').toEqual({ key: 'b', into: 20 });
    // Exactly on a boundary: the row it opens, which is the one a reader sees
    // first at that line.
    expect(anchorAt(rows, 40), "at b's top edge").toEqual({ key: 'b', into: 0 });
    // Above every row: the first one, with the edge above it.
    expect(anchorAt(rows, -30), 'above the first row').toEqual({ key: 'a', into: -30 });
    // Past the last row: nothing to hold.
    expect(anchorAt(rows, 200), 'below every row').toBeNull();
    expect(anchorAt([], 10), 'no rows at all').toBeNull();
  });

  /**
   * **The sign is the whole of this function.** A row the layout pushed DOWN
   * has to scroll the column DOWN with it, or the fix runs the drift backwards.
   */
  it('scrolls with the row it holds', () => {
    const held = { key: 'b', into: 20 };

    expect(anchoredScroll(held, 40), 'where the row was').toBe(60);
    expect(anchoredScroll(held, 140), 'a row pushed down 100').toBe(160);
    expect(anchoredScroll(held, 4), 'a row pulled up 36').toBe(24);
    // Fractional rows are laid out by the browser; the answer is whole pixels,
    // which is what a scroll offset is.
    expect(anchoredScroll(held, 40.6), 'a fractional row top').toBe(61);
  });

  /**
   * **The pair is a capture and its inverse.** What `anchorAt` reads off a row
   * has to be put back by `anchoredScroll` exactly where it was: capture at a
   * viewport top, then ask where to scroll for the same row top, and the answer
   * is that viewport top again.
   *
   * The first version of this pair subtracted, landing twice the distance into
   * the row short of the capture - and the test pinned the same wrong numbers,
   * so nothing failed. Measured in a real browser before that was believed
   * (`into` 10 -> 20px short, 100 -> 200px short). This arm is the relation
   * itself, so no sign can be wrong quietly again.
   */
  it('restores the viewport to exactly where it was captured', () => {
    for (const top of [0, 40, 40.6, 173, 900]) {
      for (const offset of [-40, 0, 20, 59]) {
        const viewTop = top + offset;
        const held = anchorAt([row('a', top, top + 60)], viewTop);
        expect(held, `the row is under the edge at ${viewTop}`).not.toBeNull();
        if (held === null) continue;
        expect(
          anchoredScroll(held, top),
          `the capture at ${viewTop} over a row at ${top} comes back`,
        ).toBe(Math.round(viewTop));
      }
    }
  });
});
