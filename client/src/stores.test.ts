import { describe, expect, it } from 'vitest';

import { Stores } from './stores';

const LEAD = { org: 'TestOrg', project: 'proj', label: 'lead' } as const;

describe('the stores one connection holds', () => {
  /**
   * A store holds the updates a page has not folded yet, and a page folds as
   * it goes. A connection stays open for a working day, so without a ceiling
   * a busy seat grows this list for as long as the app is running.
   */
  it('keeps a bounded number of updates', () => {
    const stores = new Stores();
    const store = stores.open('home');
    store.set({ projects: [] });

    for (let at = 0; at < 900; at += 1) store.push('catalog_loaded');

    // The property, not the constant: a ceiling that is any ceiling still
    // leaves the list shorter than what was pushed at it.
    expect(store.updates().length, 'the list grew with no ceiling').toBeLessThan(900);
    // The newest are the ones a page has not folded, so they are what is kept.
    expect(store.updates().length).toBeGreaterThan(0);
  });

  /** A snapshot supersedes the updates before it, so they go with it. */
  it('drops the updates a fresh snapshot replaces', () => {
    const stores = new Stores();
    const store = stores.open('home');
    store.set({ projects: [] });
    store.push('catalog_loaded');

    store.set({ projects: [1] });

    expect(store.updates()).toEqual([]);
    expect(store.state()).toEqual({ kind: 'ready' });
  });

  /**
   * One unsubscribe drops one of the two, so a subject held twice is still
   * held - and the count is what a reconnect re-asks by.
   */
  it('counts subscriptions per subject rather than per store', () => {
    const stores = new Stores();
    const seat = { session: LEAD };
    stores.open(seat);
    stores.open(seat);

    expect(stores.count(seat)).toBe(2);
    expect(stores.close(seat), 'the first unsubscribe dropped the subject').toBe(false);
    expect(stores.get(seat)).toBeDefined();
    expect(stores.close(seat)).toBe(true);
    expect(stores.get(seat)).toBeUndefined();
  });

  /**
   * A refusal is a state a page draws rather than an absence of data: `null`
   * for the snapshot means loading, refused and genuinely empty at once, and
   * a page cannot tell them apart.
   */
  it('carries a refusal beside the snapshot rather than in its place', () => {
    const stores = new Stores();
    const store = stores.refused({ session: LEAD }, 'no such seat');

    expect(store.state()).toEqual({ kind: 'refused', why: 'no such seat' });
    expect(store.snapshot()).toBeNull();
  });
});
