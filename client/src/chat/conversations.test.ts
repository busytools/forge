import { describe, expect, it } from 'vitest';

import type { Anchor } from './anchor';
import { Places } from './conversations';

const anchor = (key: string): Anchor => ({ key, into: 10 });

describe('the saved places', () => {
  /**
   * **The announced id decides, and only a moved id takes the place.** The
   * column drops a shown seat's place itself when the occupant changes, so
   * this store's own delete is what covers a seat nobody is drawing - the
   * comparison is the rule either way, and this pins it without a Chat.
   */
  it('keeps the place for the same occupant and drops it for a moved one', () => {
    const places = new Places();
    places.keep('seat', 's1', anchor('t1:r'));

    places.settle('seat', 's1');
    expect(places.of('seat'), 'the same conversation keeps it').toBeDefined();

    places.settle('seat', 's2');
    expect(places.of('seat'), 'a moved id takes it').toBeUndefined();
  });

  it('keeps a place an announcement names no id for', () => {
    const places = new Places();
    places.keep('seat', 's1', anchor('t1:r'));

    places.settle('seat', null);

    expect(places.of('seat'), 'nothing was announced to settle it').toBeDefined();
  });

  it('drops a place recorded before any id when an id is announced', () => {
    const places = new Places();
    places.keep('seat', null, anchor('t1:r'));

    places.settle('seat', 's1');

    expect(places.of('seat'), 'a replacement is the only word it has').toBeUndefined();
  });
});
