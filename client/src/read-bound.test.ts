/**
 * The bound on a subscription's first read: a subject still loading when the
 * bound runs out is asked again, once on the bound's own cadence and then on
 * the slow beat, until an answer lands.
 */
import { afterEach, describe, expect, it, vi } from 'vitest';

import { Stores } from './stores';
import { READ_BOUND_MS, REVIEW_READ_BOUND_MS, ReadBounds, SLOW_RETRY_MS } from './read-bound';
import type { Subject } from './protocol';

const HOME: Subject = 'home';

/** A `Stores` plus the bound over it, with the re-ask recorded. */
function bounded(boundMs = READ_BOUND_MS, slowMs = SLOW_RETRY_MS) {
  const stores = new Stores();
  const asked: Subject[] = [];
  const bounds = new ReadBounds(
    stores,
    (what) => {
      asked.push(what);
      return true;
    },
    boundMs,
    slowMs,
  );
  return { stores, bounds, asked };
}

afterEach(() => {
  vi.useRealTimers();
});

describe('the pass a read gets', () => {
  /**
   * **A review read gets the longer pass**, because the server bounds that
   * read itself: up to three seconds refreshing the base, up to three more
   * waiting for a scan in flight, then the encode. The ordinary pass would
   * land inside that window and ask a second whole review read under the
   * first. Reverting either caller to `arm(what)` reds this.
   */
  it('holds a review read for the longer pass, then asks once', () => {
    vi.useFakeTimers();
    const { stores, bounds, asked } = bounded();
    const store = stores.open(HOME);
    bounds.arm(HOME, REVIEW_READ_BOUND_MS);

    vi.advanceTimersByTime(READ_BOUND_MS);
    expect(asked, 'the ordinary pass does not end a review read').toEqual([]);

    vi.advanceTimersByTime(REVIEW_READ_BOUND_MS - READ_BOUND_MS);
    expect(asked, 'the longer pass does').toEqual([HOME]);

    store.set({ fleet: [] });
    bounds.clear(HOME);
  });
});

describe('a read that is not answered', () => {
  /**
   * **A slow answer buys one retry, not one per cadence** (#2561). A whole
   * record on a slow link takes longer than the bound; a fixed beat then sent a
   * fresh subscribe every cadence, each answered with another whole record
   * queued behind the one already in flight. The figures are the shipping ones
   * and the clock is driven, so the claim is about the cadence itself.
   */
  it('re-asks a slow answer once at five seconds, then leaves it alone', () => {
    vi.useFakeTimers();
    const { stores, bounds, asked } = bounded();
    const store = stores.open(HOME);
    bounds.arm(HOME);

    vi.advanceTimersByTime(5_000);
    expect(asked, 'the read is owed and nothing else would ask it').toEqual([HOME]);

    // Eighteen seconds is the slowest whole-record answer measured on the
    // link behind #2561. No further pass may stack another record behind it.
    vi.advanceTimersByTime(13_000);
    expect(asked, 'a slow answer bought a second whole re-read').toEqual([HOME]);

    // The answer lands, and the read is over: nothing is owed past it.
    store.set({ fleet: [] });
    bounds.clear(HOME);
    vi.advanceTimersByTime(120_000);
    expect(asked, 'an answered read was asked for again').toEqual([HOME]);
  });

  /**
   * **A lost answer still recovers.** The cadence pass is not the end of the
   * bound: a read nothing answers goes on being checked at the slow beat, so
   * a snapshot lost twice is not a store loading for the life of the page.
   */
  it('recovers a genuinely lost answer on the slow beat', () => {
    vi.useFakeTimers();
    const { stores, bounds, asked } = bounded();
    stores.open(HOME);
    bounds.arm(HOME);

    vi.advanceTimersByTime(5_000);
    expect(asked, 'the lost answer was not retried').toEqual([HOME]);

    vi.advanceTimersByTime(60_000);
    expect(asked, 'the read was never checked again after its cadence pass').toEqual([HOME, HOME]);

    vi.advanceTimersByTime(60_000);
    expect(asked, 'the slow beat stopped asking').toEqual([HOME, HOME, HOME]);

    bounds.stop();
  });

  it('stops once an answer lands', () => {
    vi.useFakeTimers();
    const { stores, bounds, asked } = bounded(1_000);
    const store = stores.open(HOME);
    bounds.arm(HOME);
    // The answer a snapshot is: the state leaves `loading`.
    store.set({ fleet: [] });

    vi.advanceTimersByTime(5_000);
    expect(asked, 'an answered read owes nothing').toEqual([]);
  });

  it('stops on a refusal, which is an answer', () => {
    vi.useFakeTimers();
    const { stores, bounds, asked } = bounded(1_000);
    const store = stores.open(HOME);
    bounds.arm(HOME);
    store.refuse('no home here');

    vi.advanceTimersByTime(5_000);
    expect(asked).toEqual([]);
  });

  it('stops when the last reader lets the subject go', () => {
    vi.useFakeTimers();
    const { stores, bounds, asked } = bounded(1_000);
    stores.open(HOME);
    bounds.arm(HOME);
    stores.close(HOME);

    vi.advanceTimersByTime(5_000);
    expect(asked, 'nothing is drawn, so nothing is asked for').toEqual([]);
  });

  it('stops for good when the connection does', () => {
    vi.useFakeTimers();
    const { stores, bounds, asked } = bounded(1_000);
    stores.open(HOME);
    bounds.arm(HOME);
    bounds.stop();

    vi.advanceTimersByTime(5_000);
    expect(asked, 'a closed connection has nothing to ask on').toEqual([]);
  });

  /**
   * A read on a connection that is down has nothing to ask on: the re-ask is
   * refused, and a bound that re-armed anyway would tick forever for a
   * subject nothing can answer - every subject this page watches, every five
   * seconds, for the life of a spell with no server.
   */
  it('does not re-arm when the re-ask could not be asked', () => {
    vi.useFakeTimers();
    const stores = new Stores();
    const asked: Subject[] = [];
    const bounds = new ReadBounds(
      stores,
      (what) => {
        asked.push(what);
        return false;
      },
      1_000,
    );
    stores.open(HOME);
    bounds.arm(HOME);

    vi.advanceTimersByTime(5_000);
    expect(asked, 'the bound asked the one time it was owed').toEqual([HOME]);
  });

  it('is re-armed from scratch by a later read of the same subject', () => {
    vi.useFakeTimers();
    const { stores, bounds, asked } = bounded(1_000);
    stores.open(HOME);
    bounds.arm(HOME);
    // A second ask for the same subject replaces the deadline rather than
    // stacking one on it.
    vi.advanceTimersByTime(500);
    bounds.arm(HOME);
    vi.advanceTimersByTime(600);
    expect(asked, 'the fresh deadline is the live one').toEqual([]);
    vi.advanceTimersByTime(400);
    expect(asked, 'and it fires at its own time').toEqual([HOME]);
  });
});
