import { describe, expect, it } from 'vitest';

import { heldReserve, sizes } from './row-sizes';

describe('the column memory of what each row measured', () => {
  it('keeps a height across the send cycle that removes and re-adds the row', () => {
    const rows = sizes();
    rows.remember('t9', 512);
    // The queue hold removes the row on the send and re-adds it on the drain;
    // the height must be there for the re-add, which is the frame the shake
    // rode (#1890).
    expect(rows.sizeOf('t9'), 'the row comes back at the height it had').toBe(512);
  });

  it('ignores a zero-size render, which is a layout in flight rather than a fact', () => {
    const rows = sizes();
    rows.remember('t1', 400);
    rows.remember('t1', 0);
    expect(rows.sizeOf('t1'), 'the earlier measurement stands').toBe(400);
  });

  it('forgets the rows the conversation no longer holds', () => {
    const rows = sizes();
    rows.remember('t1', 300);
    rows.remember('t2', 500);
    rows.prune(['t2', 't3']);
    expect(rows.sizeOf('t1'), 'a dropped turn goes').toBeUndefined();
    expect(rows.sizeOf('t2'), 'a held one stays').toBe(500);
  });

  it('does not forget a neighbour column, which shares no store', () => {
    const mine = sizes();
    const theirs = sizes();
    mine.remember('t1', 300);
    theirs.remember('t1', 700);
    mine.prune(['other']);
    expect(theirs.sizeOf('t1'), 'the other column keeps its own measurement').toBe(700);
  });

  it('has nothing to say about a key it never saw', () => {
    const rows = sizes();
    expect(rows.sizeOf('never')).toBeUndefined();
    expect(rows.sizeOf(null)).toBeUndefined();
  });
});

describe('what a held row reserves', () => {
  it('comes back at the height it measured, which is the shake the hold exists for', () => {
    expect(heldReserve(512), 'a measured row holds its own space').toBe('512px');
  });

  it('reserves nothing for a row that never measured, so no estimate draws as a plane', () => {
    // Ved, 2026-10-11: an unmeasured held row reserved the 386px estimate as
    // an EMPTY box and stood as a black plane for the whole queue wait - the
    // queued words are the pile's to draw, and an estimate is not a size the
    // row ever had.
    expect(heldReserve(undefined), 'nothing measured, nothing reserved').toBeUndefined();
  });
});
