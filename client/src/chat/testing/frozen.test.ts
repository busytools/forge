import { describe, expect, it } from 'vitest';

import { freeze } from './frozen';

describe('the freeze guard', () => {
  /**
   * **A collection's contents are invisible to `Object.values`**, so freezing
   * the object alone left the record's new `join` and `outcomes` writable -
   * the hole this branch closes. A write into a frozen map or set is the exact
   * failure the guard exists to turn from silence into a throw.
   */
  it('refuses a write into a frozen map or set', () => {
    const held = freeze({ calls: new Map([['a', 1]]), stopped: new Set(['t']) });

    expect(() => held.calls.set('b', 2), 'a frozen map took a write').toThrow('frozen');
    expect(() => held.calls.delete('a'), 'a frozen map took a delete').toThrow('frozen');
    expect(() => held.stopped.add('u'), 'a frozen set took a write').toThrow('frozen');
    expect(() => held.stopped.delete('t'), 'a frozen set took a delete').toThrow('frozen');
  });
});
