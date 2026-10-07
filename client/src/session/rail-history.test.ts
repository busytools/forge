import { describe, expect, it } from 'vitest';

import { chosenAfterPop, railEntry, railOnTop } from './rail-history';

describe('the covering rail takes a history step', () => {
  it('pushes its side over whatever the entry held', () => {
    expect(railEntry({ scroll: 12 })).toEqual({ scroll: 12, forgeRail: 'left' });
    expect(railEntry(null)).toEqual({ forgeRail: 'left' });
  });

  it('reads back only its own side', () => {
    expect(railOnTop({ forgeRail: 'left' })).toBe('left');
    // A route entry, a foreign flag and a malformed one are all "not ours".
    expect(railOnTop({ forgeRail: 'top' })).toBeNull();
    expect(railOnTop({ other: true })).toBeNull();
    expect(railOnTop(null)).toBeNull();
  });

  it('lands on the rail by showing it', () => {
    expect(chosenAfterPop({ forgeRail: 'left' }, true, null, null)).toEqual({ left: true });
  });

  it('keeps a closed rail closed as a press walks past its entry', () => {
    expect(chosenAfterPop({ forgeRail: 'left' }, true, 'left', 'left')).toEqual({ left: false });
  });

  it('closes it where it covers when the pop leaves the entry', () => {
    expect(chosenAfterPop({}, true, null, null)).toEqual({ left: false });
    expect(chosenAfterPop({}, true, 'left', null)).toEqual({ left: false });
  });

  it('at a wide width leaves the column alone, except where the pop left its entry', () => {
    // The wide case is the one a columned rail must survive: a route pop is
    // not an overlay closing.
    expect(chosenAfterPop({}, false, null, null)).toBeNull();
    // The band opening back up under an open rail: the pop still closes the
    // side whose entry it left.
    expect(chosenAfterPop({}, false, 'left', null)).toEqual({ left: false });
    expect(chosenAfterPop({ forgeRail: 'left' }, false, null, null)).toEqual({ left: true });
  });
});
