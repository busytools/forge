import { describe, expect, it } from 'vitest';

import { chosenAfterPop, railEntry, railOnTop } from './rail-history';

describe('the covering rail takes a history step', () => {
  it('pushes its side over whatever the entry held', () => {
    expect(railEntry({ scroll: 12 }, 'left')).toEqual({ scroll: 12, forgeRail: 'left' });
    expect(railEntry(null, 'right')).toEqual({ forgeRail: 'right' });
  });

  it('reads back only its own sides', () => {
    expect(railOnTop({ forgeRail: 'left' })).toBe('left');
    expect(railOnTop({ forgeRail: 'right' })).toBe('right');
    // A route entry, a foreign flag and a malformed one are all "not ours".
    expect(railOnTop({ forgeRail: 'top' })).toBeNull();
    expect(railOnTop({ other: true })).toBeNull();
    expect(railOnTop(null)).toBeNull();
  });

  it('lands on a side by showing it and stepping out of the other', () => {
    expect(chosenAfterPop({ forgeRail: 'left' }, true, null, null)).toEqual({
      left: true,
      right: false,
    });
    expect(chosenAfterPop({ forgeRail: 'right' }, true, null, null)).toEqual({
      left: false,
      right: true,
    });
  });

  it('keeps a closed rail closed as a press walks past its entry', () => {
    expect(chosenAfterPop({ forgeRail: 'left' }, true, 'right', 'left')).toEqual({
      left: false,
      right: false,
    });
  });

  it('closes both where a rail covers when the pop leaves the entry', () => {
    expect(chosenAfterPop({}, true, null, null)).toEqual({ left: false, right: false });
    expect(chosenAfterPop({}, true, 'left', null)).toEqual({ left: false, right: false });
  });

  it('at a wide width leaves the columns alone, except one whose entry the pop left', () => {
    // The wide case is the one a columned rail must survive: a route pop is
    // not an overlay closing.
    expect(chosenAfterPop({}, false, null, null)).toBeNull();
    // The band opening back up under an open rail: the pop still closes the
    // side whose entry it left, and touches no column but that one.
    expect(chosenAfterPop({}, false, 'left', null)).toEqual({ left: false, right: null });
    expect(chosenAfterPop({ forgeRail: 'left' }, false, null, null)).toEqual({
      left: true,
      right: null,
    });
  });
});
