/**
 * The rate-limit line's port fidelity.
 *
 * Every branch `app/events/rate_limit.rs` pins in its own tests is pinned
 * here word for word, so a drift on either side is a red test rather than two
 * views wording one window differently.
 */

import { afterEach, describe, expect, it, vi } from 'vitest';

import { formatRateLimitSummary, formatResetsAt, rateLimitNoticeKey } from './rate-limit';

/** A window that resets at 13:00 UTC, from the capture's own frame. */
const RESETS_AT = 1_790_168_400;

afterEach(() => {
  vi.useRealTimers();
});

/** Fix the wall clock so the countdown is part of the expected words. */
function at(secondsBeforeReset: number): void {
  vi.useFakeTimers();
  vi.setSystemTime(new Date((RESETS_AT - secondsBeforeReset) * 1000));
}

describe('the resets-at clock', () => {
  it('reads a countdown and the UTC wall-clock', () => {
    at(15_780); // 4h 23m
    expect(formatResetsAt(RESETS_AT)).toBe('4h 23m at 13:00 UTC');
  });

  it('reads whole minutes when under an hour, and under a minute at the door', () => {
    at(300);
    expect(formatResetsAt(RESETS_AT)).toBe('5m at 13:00 UTC');
    at(59);
    expect(formatResetsAt(RESETS_AT)).toBe('< 1 minute at 13:00 UTC');
  });

  it('reads now for a window that has passed', () => {
    at(-1);
    expect(formatResetsAt(RESETS_AT)).toBe('now at 13:00 UTC');
  });

  it('reads a bare now for a timestamp the clock cannot use', () => {
    // The terminal's guard on the same input: a negative or non-finite
    // epoch must not throw on the way to a countdown.
    expect(formatResetsAt(-1)).toBe('now');
    expect(formatResetsAt(Number.NaN)).toBe('now');
    expect(formatResetsAt(Number.POSITIVE_INFINITY)).toBe('now');
  });
});

describe('the summary, branch for branch with the terminal', () => {
  it('asks for extra usage when a rejected window has nothing else to say', () => {
    const summary = formatRateLimitSummary({
      status: 'rejected',
      overageDisabledReason: 'org_level_disabled',
      isUsingOverage: false,
    });
    expect(summary).toBe(
      'Extra usage credit is required to continue. Use /extra-usage to enable it, /model to switch models, or wait for the rate-limit window to reset.',
    );
  });

  it('keeps the normal words when the same rejection names its window', () => {
    at(15_780);
    const summary = formatRateLimitSummary({
      status: 'rejected',
      resetsAt: RESETS_AT,
      rateLimitType: 'five_hour',
      overageDisabledReason: 'org_level_disabled',
      isUsingOverage: false,
    });
    expect(summary).toBe(
      "Rate limit reached, you've hit your 5-hour rate limit. Resets in 4h 23m at 13:00 UTC.",
    );
  });

  it('softens a threshold crossed without consuming overage', () => {
    at(15_780);
    const summary = formatRateLimitSummary({
      status: 'allowed_warning',
      resetsAt: RESETS_AT,
      utilization: 1.02,
      rateLimitType: 'overage',
      isUsingOverage: false,
      surpassedThreshold: 1,
    });
    expect(summary).toBe('Near rate-limit threshold. Resets in 4h 23m at 13:00 UTC.');
    // No percentage and no overage hint: the loud words would over-state a
    // threshold that is not being consumed.
    expect(summary).not.toContain('%');
    expect(summary).not.toContain('overage allowance');
  });

  it('keeps the loud words when the threshold is crossed with overage in use', () => {
    at(15_780);
    const summary = formatRateLimitSummary({
      status: 'allowed_warning',
      resetsAt: RESETS_AT,
      utilization: 1.05,
      rateLimitType: 'overage',
      isUsingOverage: true,
      surpassedThreshold: 1,
    });
    expect(summary).toBe(
      "Approaching rate limit, you've used 105% of your overage rate limit. Resets in 4h 23m at 13:00 UTC.",
    );
  });

  it('offers the overage allowance when the wire says one exists', () => {
    at(15_780);
    const summary = formatRateLimitSummary({
      status: 'allowed_warning',
      resetsAt: RESETS_AT,
      utilization: 0.91,
      overageStatus: 'allowed',
    });
    expect(summary).toBe(
      "Approaching rate limit, you've used 91% of your rate limit. You can continue using your overage allowance. Resets in 4h 23m at 13:00 UTC.",
    );
  });

  it('rounds a percentage as the terminal does, ties to even', () => {
    // Rust's `{:.0}`: 0.125 * 100 is exactly 12.5, which rounds to 12 where
    // JavaScript's half-up round would say 13.
    const summary = formatRateLimitSummary({
      status: 'allowed_warning',
      utilization: 0.125,
      rateLimitType: 'five_hour',
    });
    expect(summary).toBe("Approaching rate limit, you've used 12% of your 5-hour rate limit.");
  });

  it('offers overage only when the wire carries a status', () => {
    // The terminal's `overage_status.is_some()`: an explicit null is not a
    // carried value, so no allowance is offered for it.
    const summary = formatRateLimitSummary({
      status: 'allowed_warning',
      overageStatus: null,
    });
    expect(summary).toBe("Approaching rate limit, you've hit your rate limit.");
  });

  it('states overage use on a rejection that is consuming it', () => {
    at(15_780);
    const summary = formatRateLimitSummary({
      status: 'rejected',
      resetsAt: RESETS_AT,
      utilization: 1.1,
      rateLimitType: 'five_hour',
      isUsingOverage: true,
    });
    expect(summary).toBe(
      "Rate limit reached, you've used 110% of your 5-hour rate limit. You are using your overage allowance. Resets in 4h 23m at 13:00 UTC.",
    );
  });
});

describe('the incident key', () => {
  it('names the same key for the same window however its status moves', () => {
    const warning = rateLimitNoticeKey({ rateLimitType: 'five_hour', resetsAt: RESETS_AT });
    const rejected = rateLimitNoticeKey({
      status: 'rejected',
      rateLimitType: 'five_hour',
      resetsAt: RESETS_AT,
    });
    expect(rejected).toBe(warning);
  });

  it('opens a new key when the window resets', () => {
    const first = rateLimitNoticeKey({ rateLimitType: 'five_hour', resetsAt: RESETS_AT });
    const next = rateLimitNoticeKey({ rateLimitType: 'five_hour', resetsAt: RESETS_AT + 18_000 });
    expect(next).not.toBe(first);
  });

  it('keeps two types apart at the same reset instant', () => {
    const fiveHour = rateLimitNoticeKey({ rateLimitType: 'five_hour', resetsAt: RESETS_AT });
    const sevenDay = rateLimitNoticeKey({ rateLimitType: 'seven_day', resetsAt: RESETS_AT });
    expect(sevenDay, 'the window type is part of the incident').not.toBe(fiveHour);
  });
  it('stands on its own when the frame names neither window nor reset', () => {
    expect(rateLimitNoticeKey({ status: 'rejected' })).toBe('rate-limit:any:none');
  });
});
