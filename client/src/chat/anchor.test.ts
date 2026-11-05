import { describe, expect, it } from 'vitest';

import { anchoredScroll, anchorAt, heldRow, type RowBox } from './anchor';

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
    // Fractional rows are laid out by the browser, and a frozen column keeps
    // the fraction: rounding it away moves the row under the reader's eye.
    expect(anchoredScroll(held, 40.6), 'a fractional row top').toBe(60.6);
  });

  /**
   * **Only the rows up to the edge are measured.** The caller hands a generator
   * of boxes, and measuring one is a layout read on every scroll event of the
   * giant seat: buffering the whole window would survive the code without this
   * arm, and the doc's claim would have no witness.
   */
  it('measures rows only until the edge is found', () => {
    let asked = 0;
    function* rows(): Generator<RowBox> {
      for (let at = 0; at < 5; at += 1) {
        asked += 1;
        yield row(`r${at}`, at * 40, at * 40 + 40);
      }
    }

    // The edge is inside the second row: the three below it are never asked of
    // the layout, which is the whole point of taking an iterable.
    expect(anchorAt(rows(), 50), 'the row under the edge').toEqual({ key: 'r1', into: 10 });
    expect(asked, 'the scan stopped at the edge').toBe(2);
  });

  /**
   * **A row below the top edge is a place only when it is still on screen.**
   * A jump can fire its event before the list has re-windowed: every drawn row
   * then sits below the edge, and the first of them is thousands of pixels
   * from the reader's eye - a place that would restore them to a stranger's
   * row. A gap narrower than the room is the column's own margin, and the row
   * below it is what the reader is looking at.
   */
  it('holds a row under the edge, or the nearest one still on screen', () => {
    const rows = [row('a', 0, 40), row('b', 40, 90)];

    expect(heldRow(rows, 60, 300), 'the edge inside a row').toEqual({ key: 'b', into: 20 });
    expect(heldRow(rows, -20, 300), 'a gap inside the room').toEqual({ key: 'a', into: -20 });
    expect(heldRow(rows, -400, 300), 'a window that has not caught up').toBeNull();
    expect(heldRow([], 10, 300), 'no rows drawn at all').toBeNull();
  });

  /**
   * **The capture keeps the fraction too, not only the restore.** The pair's
   * round trip comes out integral on whole rows, so a rounded capture and a
   * rounded restore agree there; a fractional edge over a fractional row is
   * where the capture's own rounding shows.
   */
  it('keeps the fraction of the distance into the row', () => {
    expect(anchorAt([row('a', 40.5, 90)], 50.25), 'a fractional edge').toEqual({
      key: 'a',
      into: 9.75,
    });
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
        ).toBe(viewTop);
      }
    }
  });
});
