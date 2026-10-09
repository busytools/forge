import { describe, expect, it } from 'vitest';

import { prune, remember, sizeOf } from './row-sizes';

describe('the column memory of what each row measured', () => {
  it('keeps a height across the send cycle that removes and re-adds the row', () => {
    remember('t9', 512);
    // The queue hold removes the row on the send and re-adds it on the drain;
    // the height must be there for the re-add, which is the frame the shake
    // rode (#1890).
    expect(sizeOf('t9'), 'the row comes back at the height it had').toBe(512);
  });

  it('ignores a zero-size render, which is a layout in flight rather than a fact', () => {
    remember('t1', 400);
    remember('t1', 0);
    expect(sizeOf('t1'), 'the earlier measurement stands').toBe(400);
  });

  it('forgets the rows the conversation no longer holds', () => {
    remember('t1', 300);
    remember('t2', 500);
    prune(['t2', 't3']);
    expect(sizeOf('t1'), 'a dropped turn goes').toBeUndefined();
    expect(sizeOf('t2'), 'a held one stays').toBe(500);
  });

  it('has nothing to say about a key it never saw', () => {
    expect(sizeOf('never')).toBeUndefined();
    expect(sizeOf(null)).toBeUndefined();
  });
});
