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

  it('shows the side a pop landed on and steps out of the other', () => {
    expect(chosenAfterPop({ forgeRail: 'left' }, true)).toEqual({ left: true, right: false });
    expect(chosenAfterPop({ forgeRail: 'right' }, true)).toEqual({ left: false, right: true });
  });

  it('closes both where a rail covers, and says nothing at a wide width', () => {
    expect(chosenAfterPop({}, true)).toEqual({ left: false, right: false });
    // The wide case is the one a columned rail must survive: a route pop is
    // not an overlay closing.
    expect(chosenAfterPop({}, false)).toBeNull();
  });
});
