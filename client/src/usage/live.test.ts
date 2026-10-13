import { get } from 'svelte/store';
import { describe, expect, it, vi } from 'vitest';

import { PRICING_REASK_LIMIT, PRICING_RETRY_MS, watchUsage } from './live';
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
   * **The re-ask loop, because the server's answer to an unpriced scan is a
   * background fetch that nothing pushes.** The first re-ask answers
   * unpriced again while the fetch is still out - the trip the fetch's 20s
   * budget makes real - and the next one, after the fetch has landed, is
   * the priced report. Asking stops the moment one prices.
   */
  it('keeps asking while the report is unpriced, and stops the moment it prices', () => {
    vi.useFakeTimers();
    try {
      const forge = fakeConnection();
      const view = watchUsage(forge.connection);
      const stop = view.subscribe(() => {});
      const unpriced = (): void => {
        forge.arrive({
          kind: 'snapshot',
          subject: USAGE,
          data: { ...usageWire, pricing_available: false },
        });
      };

      unpriced();
      expect(forge.refreshed, 'the page asked before the re-price delay').toEqual([]);

      vi.advanceTimersByTime(PRICING_RETRY_MS);
      expect(forge.refreshed, 'the unpriced answer was never asked again').toEqual([USAGE]);

      // **A fetch slower than one re-ask reaches**: the first re-ask answers
      // unpriced again, and the loop must not stop there.
      unpriced();
      vi.advanceTimersByTime(PRICING_RETRY_MS);
      expect(forge.refreshed, 'a slow fetch was abandoned after one re-ask').toEqual([
        USAGE,
        USAGE,
      ]);

      // The fetch lands; the next answer is priced, and asking ends.
      forge.arrive({ kind: 'snapshot', subject: USAGE, data: usageWire });
      vi.advanceTimersByTime(PRICING_RETRY_MS * 6);
      expect(forge.refreshed, 'asking continued past the priced answer').toEqual([USAGE, USAGE]);
      stop();
    } finally {
      vi.useRealTimers();
    }
  });

  /** **The loop is bounded**: a forge whose fetch never lands is asked the
   * ceiling and no further - past it the caption is terminal, and more asks
   * would be a poll. */
  it('stops at the ceiling when the report never prices', () => {
    vi.useFakeTimers();
    try {
      const forge = fakeConnection();
      const stop = watchUsage(forge.connection).subscribe(() => {});

      for (let round = 0; round <= PRICING_REASK_LIMIT + 2; round += 1) {
        forge.arrive({
          kind: 'snapshot',
          subject: USAGE,
          data: { ...usageWire, pricing_available: false },
        });
        vi.advanceTimersByTime(PRICING_RETRY_MS);
      }

      expect(forge.refreshed.length, 'an always-unpriced forge was polled').toBe(
        PRICING_REASK_LIMIT,
      );
      stop();
    } finally {
      vi.useRealTimers();
    }
  });

  /**
   * **A priced report drops a pending re-ask.** The loop's goal is reached,
   * so the scan still scheduled is one the reader did not make.
   */
  it('drops a pending re-ask the moment a priced report arrives', () => {
    vi.useFakeTimers();
    try {
      const forge = fakeConnection();
      const stop = watchUsage(forge.connection).subscribe(() => {});
      forge.arrive({
        kind: 'snapshot',
        subject: USAGE,
        data: { ...usageWire, pricing_available: false },
      });

      // Priced before the timer fires (a re-subscribe, or the reader's own
      // refresh).
      forge.arrive({ kind: 'snapshot', subject: USAGE, data: usageWire });
      vi.advanceTimersByTime(PRICING_RETRY_MS * 2);
      expect(forge.refreshed, 'a priced report left a re-ask scheduled').toEqual([]);
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
