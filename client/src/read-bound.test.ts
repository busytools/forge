/**
 * The bound on a subscription's first read: a subject that is still loading
 * when the bound runs out is asked again, and the bound re-arms until an
 * answer lands.
 */
import { afterEach, describe, expect, it, vi } from 'vitest';

import { Stores } from './stores';
import { READ_BOUND_MS, ReadBounds } from './read-bound';
import type { Subject } from './protocol';

const HOME: Subject = 'home';

/** A `Stores` plus the bound over it, with the re-ask recorded. */
function bounded(boundMs = READ_BOUND_MS) {
  const stores = new Stores();
  const asked: Subject[] = [];
  const bounds = new ReadBounds(
    stores,
    (what) => {
      asked.push(what);
      return true;
    },
    boundMs,
  );
  return { stores, bounds, asked };
}

afterEach(() => {
  vi.useRealTimers();
});

describe('a read that is not answered', () => {
  it('is asked again when the bound runs out, and the bound re-arms', () => {
    vi.useFakeTimers();
    const { stores, bounds, asked } = bounded(1_000);
    stores.open(HOME);
    bounds.arm(HOME);

    vi.advanceTimersByTime(1_000);
    expect(asked, 'the read is owed and nothing else would ask it').toEqual([HOME]);

    // A second loss is the same loss: the re-arm means the ask is bounded
    // too, rather than one retry that leaves the store loading forever.
    vi.advanceTimersByTime(1_000);
    expect(asked, 'the re-ask is bounded like the first read').toEqual([HOME, HOME]);
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
