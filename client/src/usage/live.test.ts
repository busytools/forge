import { get } from 'svelte/store';
import { describe, expect, it, vi } from 'vitest';

import { PRICING_RETRY_MS, watchUsage } from './live';
import { fakeConnection, USAGE, usageWire } from './testing';

describe('the usage page over a connection', () => {
  it('subscribes the usage subject for as long as it is watched', () => {
    const forge = fakeConnection();
    const stop = watchUsage(forge.connection).subscribe(() => {});

    expect(forge.subscribed).toEqual([USAGE]);
    stop();
    expect(forge.unsubscribed).toEqual([USAGE]);
  });

  /**
   * The subscription's own answer is the read, narrowed at the boundary
   * before any page sees it.
   */
  it('takes the snapshot the subscription is answered with', () => {
    const forge = fakeConnection();
    const view = watchUsage(forge.connection);
    const stop = view.subscribe(() => {});

    forge.arrive({ kind: 'snapshot', subject: USAGE, data: usageWire });

    const held = get(view);
    expect(held.wire?.lifetime.total.input).toBe(2_500_000);
    expect(held.wire?.pricingAvailable).toBe(true);
    expect(held.refused).toBeNull();
    stop();
  });

  /**
   * **The subject hears nothing after its first scan**, so the read is asked
   * for again rather than waited on - a page that sat for updates would sit
   * for ever. The re-ask is the page's own (its refresh control), which
   * arrives through the same snapshot path; nothing rides the arrival
   * itself. The one deliberate self-ask is the unpriced re-price, and it is
   * a delay away (its own case below).
   */
  it('takes a re-read the same way, and asks nothing on its own', () => {
    const forge = fakeConnection();
    const view = watchUsage(forge.connection);
    const stop = view.subscribe(() => {});
    forge.arrive({ kind: 'snapshot', subject: USAGE, data: usageWire });

    // The page's refresh control ends as another snapshot on the same
    // subject; it is taken the same way.
    forge.arrive({
      kind: 'snapshot',
      subject: USAGE,
      data: { ...usageWire, pricing_available: false },
    });
    expect(get(view).wire?.pricingAvailable).toBe(false);

    expect(forge.refreshed, 'a watch that re-asked by itself would poll the pool').toEqual([]);
    stop();
  });

  /**
   * **An unpriced answer is asked once more, and only once.** The server
   * kicks its pricing refill off the scan that answered, and nothing pushes
   * - so the priced report is only ever seen by asking again, which is the
   * terminal's own re-price translated. A retry against a forge that cannot
   * fetch stops after the one ask rather than polling.
   */
  it('asks once more when the report arrives unpriced', () => {
    vi.useFakeTimers();
    try {
      const forge = fakeConnection();
      const view = watchUsage(forge.connection);
      const stop = view.subscribe(() => {});

      forge.arrive({
        kind: 'snapshot',
        subject: USAGE,
        data: { ...usageWire, pricing_available: false },
      });
      expect(forge.refreshed, 'the page asked before the re-price delay').toEqual([]);

      vi.advanceTimersByTime(PRICING_RETRY_MS);
      expect(forge.refreshed, 'the unpriced answer was never asked again').toEqual([USAGE]);

      // The fetch failed, or took past the delay: a second unpriced answer
      // starts no loop.
      forge.arrive({
        kind: 'snapshot',
        subject: USAGE,
        data: { ...usageWire, pricing_available: false },
      });
      vi.advanceTimersByTime(PRICING_RETRY_MS * 5);
      expect(forge.refreshed, 'the retry became a poll').toEqual([USAGE]);
      stop();
    } finally {
      vi.useRealTimers();
    }
  });

  /** A page left before the delay must not ask for a scan nobody draws. */
  it('drops the pending re-ask when the watch is released', () => {
    vi.useFakeTimers();
    try {
      const forge = fakeConnection();
      const stop = watchUsage(forge.connection).subscribe(() => {});
      forge.arrive({
        kind: 'snapshot',
        subject: USAGE,
        data: { ...usageWire, pricing_available: false },
      });

      stop();
      vi.advanceTimersByTime(PRICING_RETRY_MS * 2);
      expect(forge.refreshed, 'a released watch asked for a scan').toEqual([]);
    } finally {
      vi.useRealTimers();
    }
  });

  it('carries the server refusal in its own words', () => {
    const forge = fakeConnection();
    const view = watchUsage(forge.connection);
    const stop = view.subscribe(() => {});

    forge.arrive({ kind: 'error', what: 'subscribe', why: 'that subject is not served here' });

    expect(get(view).refused).toBe('that subject is not served here');
    expect(get(view).wire).toBeNull();
    stop();
  });
});
