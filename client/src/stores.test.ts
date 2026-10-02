import { describe, expect, it } from 'vitest';

import type { SessionUpdate } from './protocol';
import { REPLACES } from './session/apply';
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

  /**
   * A seat's store is keyed by the seat, and what it holds belongs to one
   * occupant. A replacement that lands while nobody is reading it therefore
   * has to leave nothing behind: a fresh mount seeds from the snapshot and
   * replays the frames (the chat's `heldRunning`), so a store still holding
   * the last occupant's answer draws its running state - `turn_in_flight`
   * here - over the new occupant's newest row until the seat's own answer
   * lands a round trip later.
   */
  it('holds nothing of the occupant a seat was replaced from', () => {
    // Written out rather than read off `REPLACES`, following `live.test.ts`:
    // a loop over the list under test cannot see the list change.
    const replacing = ['spawning', 'connected', 'history_replayed', 'session_replaced'];
    expect(
      [...REPLACES].sort(),
      'the list the store reads is not the list this test covers',
    ).toEqual([...replacing].sort());
    for (const name of replacing) {
      const stores = new Stores();
      const seat = { session: LEAD };
      const store = stores.open(seat);
      // Someone visited the seat: answered mid-turn, then stepped by its
      // frames - and the swap lands with nobody reading, which is the case.
      store.set({ header: { turn_in_flight: true } });
      store.push(opening());
      // A tail long enough to overflow the store's ceiling, so the store is
      // holding a dropped count when the replacement lands. The clear has to
      // reset it: a reader suppresses its replay while that count is nonzero,
      // and the new occupant's own frames are then never read.
      for (let at = 0; at < 520; at += 1) store.push('catalog_loaded');

      store.push(occupantAs(name));

      expect(
        store.snapshot(),
        `${name}: the last occupant's record was there to seed from`,
      ).toBeNull();
      expect(store.updates(), `${name}: the last occupant's frames were there to replay`).toEqual(
        [],
      );
      expect(store.state(), `${name}: the seat's store was not put back to loading`).toEqual({
        kind: 'loading',
      });
      expect(store.dropped(), `${name}: the cleared store kept the old tail's count`).toBe(0);

      // And the seat's store is empty rather than dead: the next occupant's
      // own frames land in it.
      store.push(opening('the-one-that-arrived'));
      expect(store.updates(), `${name}: the store took no frames after the swap`).toHaveLength(1);
    }

    // The fleet's own store holds a different subject: a seat's swap is news
    // for the home rather than a replacement of what the home holds, so the
    // rule stops at seats.
    const fleet = new Stores();
    const home = fleet.open('home');
    home.set({ projects: [] });
    home.push(occupantAs('connected'));
    expect(home.snapshot(), 'a seat swap emptied the home store').toEqual({ projects: [] });
  });
});

/** The frame a turn opens with, which is what a replay reads a turn out of. */
function opening(sessionId = 'the-one-that-left'): SessionUpdate {
  return {
    chat_appended: {
      key: LEAD,
      msg: { type: 'system', subtype: 'init', session_id: sessionId, tools: [] },
    },
  };
}

/**
 * A seat taking a new occupant, under any of the four names that replace it.
 *
 * One payload for all four: what the store's rule reads is the variant's
 * name, and the fields past the slot are the page's business.
 */
function occupantAs(name: string): SessionUpdate {
  return {
    [name]: {
      key: LEAD,
      session_id: 'the-one-that-left',
      cwd: '/tmp',
      current_model: null,
      available_models: [],
      mode: null,
      history: [],
      compaction_count: 0,
    },
  };
}
