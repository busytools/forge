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
    expect(chosenAfterPop({ forgeRail: 'left' }, null, null)).toEqual({ left: true });
  });

  it('keeps a closed rail closed as a press walks past its entry', () => {
    expect(chosenAfterPop({ forgeRail: 'left' }, 'left', 'left')).toEqual({ left: false });
  });

  it('closes it when the pop leaves the entry it opened', () => {
    expect(chosenAfterPop({}, 'left', null)).toEqual({ left: false });
  });

  it('says nothing for a pop that landed on someone else entirely', () => {
    // A route entry with no rail of ours left: the rail is already away.
    expect(chosenAfterPop({}, null, null)).toBeNull();
    expect(chosenAfterPop({ other: true }, null, null)).toBeNull();
  });
});
