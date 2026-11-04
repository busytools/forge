import { describe, expect, it } from 'vitest';

import { runningAt } from './chat/conversation';
import type { SessionUpdate } from './protocol';
import { inFlightOf, REPLACES, variantOf } from './session/apply';
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

  /**
   * A seat's tail has one reader, `runningAt`, and it folds only the frames
   * that move the in-flight answer. Once no page holds the seat, a frame that
   * moves nothing can be dropped without changing that answer - and a busy
   * seat would otherwise keep every tool result until the next whole read.
   */
  it('keeps only the seat frames the running answer folds once the page has left', () => {
    const stores = new Stores();
    const seat = { session: LEAD };
    const store = stores.open(seat);
    store.set({ header: { turn_in_flight: false } });
    store.compact();

    for (let at = 0; at < 500; at += 1) store.push(spoke(`reading-${at}`, at));
    expect(store.updates().length, 'the seat tail kept frames nothing reads').toBeLessThan(5);
    expect(store.dropped(), 'the cap counted frames it did not drop').toBe(0);

    // The retained tail still answers the question `runningAt` asks: the
    // newest edge wins, whatever the traffic after it.
    store.push(opening('s'));
    expect(foldTail(store.updates(), false)).toBe(true);
    store.push(spoke('still going', 501));
    expect(foldTail(store.updates(), false)).toBe(true);
    store.push(resultFrame());
    store.push(spoke('after the result', 502));
    expect(foldTail(store.updates(), false)).toBe(false);
    expect(store.updates().length).toBeLessThan(5);
  });

  /**
   * **A busy seat is the one whose ceiling has dropped.** The drain must leave
   * a tail `runningAt` still reads: the ceiling's count describes a list that
   * is gone, and a stale snapshot would answer in its place.
   */
  it('compacts a busy tail to the frames its readers fold, and runningAt answers from it', () => {
    const stores = new Stores();
    const seat = { session: LEAD };
    const store = stores.open(seat);
    // A stale whole read: the snapshot says nothing is running.
    store.set({ header: { turn_in_flight: false } });
    // A busy seat overflows the ceiling, so the tail alone is incomplete.
    for (let at = 0; at < 600; at += 1) store.push(spoke(`busy-${at}`, at));
    expect(store.dropped(), 'precondition: the tail overflowed the ceiling').toBeGreaterThan(0);

    // The page leaves mid-turn; the newest edge is the turn opening.
    store.push(opening('s'));
    store.compact();

    expect(store.dropped(), 'the compacted tail still read as incomplete').toBe(0);
    expect(store.updates().length, 'a left seat kept frames nothing reads').toBeLessThan(5);
    const connection = { store: () => store };
    expect(runningAt(connection, LEAD, false), 'runningAt read the stale snapshot').toBe(true);
  });

  /**
   * The tail's two readers fold two variants: `runningAt` the newest chat
   * edge, the voice control its listener state. Everything else - a file
   * index amongst it - is no reader's and goes.
   */
  it('keeps only the variants its readers fold after the page has left', () => {
    const stores = new Stores();
    const seat = { session: LEAD };
    const store = stores.open(seat);
    store.set({ header: { turn_in_flight: false } });
    store.push(fileIndexFrame());
    store.push(voiceState(1));
    store.push(spoke('busy', 1));
    store.push(opening('s'));
    store.compact();

    for (let at = 0; at < 100; at += 1) store.push(spoke(`after-${at}`, at));
    store.push(fileIndexFrame());
    store.push(voiceState(2));
    store.push(resultFrame());

    const kinds = store.updates().map((update) => variantOf(update)[0]);
    expect(kinds, 'the tail is not exactly the newest voice state and the newest edge').toEqual([
      'voice_mode_state',
      'chat_appended',
    ]);
    expect(store.updates()[0], 'the newest voice state was not the one kept').toEqual(
      voiceState(2),
    );
    expect(store.updates()[1], 'an older edge stayed').toEqual(resultFrame());
    expect(foldTail(store.updates(), false), 'the newest edge did not answer').toBe(false);
  });

  /**
   * **Voice traffic must not re-open the ceiling.** The compacted tail keeps
   * one voice state and one edge, so a seat whose voice toggles for hours
   * still leaves `runningAt` a tail it can answer from.
   */
  it('bounds a voice-busy tail, and runningAt still answers from it', () => {
    const stores = new Stores();
    const seat = { session: LEAD };
    const store = stores.open(seat);
    store.set({ header: { turn_in_flight: false } });
    store.compact();
    store.push(opening('s'));
    for (let at = 0; at < 1_000; at += 1) store.push(voiceState(at));

    expect(store.updates().length, 'voice states grew the compacted tail').toBeLessThanOrEqual(2);
    expect(
      store.updates().some((update) => variantOf(update)[0] === 'voice_mode_state'),
      'the newest voice state was dropped',
    ).toBe(true);
    expect(JSON.stringify(store.updates()), 'an old voice state stayed').not.toContain(
      '"listeners":998',
    );
    expect(store.dropped(), 'voice pressure raised the ceiling count').toBe(0);
    expect(
      runningAt({ store: () => store }, LEAD, false),
      'runningAt read the stale snapshot',
    ).toBe(true);
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
    const replacing = ['spawning', 'connected', 'session_replaced'];
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
      expect(store.dropped(), 'precondition: the tail overflowed the ceiling').toBeGreaterThan(0);

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

/** An ordinary assistant frame: traffic the running answer does not fold. */
function spoke(text: string, at = 0): SessionUpdate {
  return {
    chat_appended: {
      key: LEAD,
      msg: {
        type: 'assistant',
        uuid: `a-${at}`,
        message: { role: 'assistant', content: [{ type: 'text', text }] },
      },
    },
  };
}

/** The frame a turn ends on. */
function resultFrame(): SessionUpdate {
  return { chat_appended: { key: LEAD, msg: { type: 'result', subtype: 'success' } } };
}

/** A file-index push, far larger than anything either reader folds. */
function fileIndexFrame(): SessionUpdate {
  return {
    file_index_changed: {
      key: LEAD,
      index: {
        root: '/tmp',
        files: Array.from({ length: 5_000 }, (_, at) => ({
          path: `crates/file-${at}.rs`,
          size: 1_000 + at,
          mtime: at,
        })),
      },
    },
  };
}

/** The voice control's listener count, which its reader folds. */
function voiceState(listeners: number): SessionUpdate {
  return { voice_mode_state: { key: LEAD, listeners } };
}

/** What `runningAt` would answer from a tail, folded the way it folds it. */
function foldTail(updates: SessionUpdate[], from: boolean): boolean {
  let held = from;
  for (const update of updates) {
    const message = (update as { chat_appended?: { msg?: unknown } }).chat_appended?.msg;
    if (message !== undefined) held = inFlightOf(held, message);
  }
  return held;
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
