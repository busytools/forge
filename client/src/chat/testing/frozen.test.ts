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

  /**
   * **A caller may freeze twice.** `Object.freeze` on a plain object is a
   * no-op the second time, so a publish path that freezes a value it has
   * already frozen is entitled to do so - the collection shims are what would
   * throw on the second pass.
   */
  it('takes a second pass over a value it already froze', () => {
    const held = { calls: new Map([['a', 1]]), stopped: new Set(['t']) };
    freeze(held);

    expect(() => freeze(held), 'a second freeze threw on the frozen collection').not.toThrow();
  });

  /** A value pointing at itself must not walk its own cycle to the stack's end. */
  it('freezes a value that points at itself', () => {
    const loop: { self?: unknown } = {};
    loop.self = loop;

    expect(() => freeze(loop), 'a value pointing at itself overflowed the guard').not.toThrow();
  });
});
