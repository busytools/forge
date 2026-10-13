import { describe, expect, it } from 'vitest';

import { usageFrom } from './usage';

/**
 * One window as the server's serializer writes it: snake_case, five token
 * fields, and a `TOTAL` row alongside the two groupings.
 */
const WINDOW = {
  by_model: [
    {
      label: 'claude-opus-5',
      input: 1_800_000,
      cache_write_1h: 12_000_000,
      cache_write_5m: 0,
      cache_read: 228_000_000,
      output: 900_000,
      cost_usd: 115.0,
    },
  ],
  by_project: [
    {
      label: 'forge',
      input: 1_800_000,
      cache_write_1h: 12_000_000,
      cache_write_5m: 0,
      cache_read: 228_000_000,
      output: 900_000,
      cost_usd: 115.0,
    },
  ],
  total: {
    label: 'TOTAL',
    input: 1_800_000,
    cache_write_1h: 12_000_000,
    cache_write_5m: 0,
    cache_read: 228_000_000,
    output: 900_000,
    cost_usd: 115.0,
  },
};

describe('the usage read', () => {
  it('narrows the four windows and their rows', () => {
    const report = usageFrom({
      today: WINDOW,
      week: WINDOW,
      month: WINDOW,
      lifetime: WINDOW,
      pricing_available: true,
    });

    expect(report.pricingAvailable).toBe(true);
    expect(report.lifetime.byModel[0]?.label).toBe('claude-opus-5');
    expect(report.lifetime.byProject[0]?.label).toBe('forge');
    expect(report.lifetime.total.input).toBe(1_800_000);
    expect(report.lifetime.total.cacheWrite1h).toBe(12_000_000);
    expect(report.lifetime.total.cacheRead).toBe(228_000_000);
    expect(report.lifetime.total.output).toBe(900_000);
    expect(report.lifetime.total.costUsd).toBe(115.0);
    expect(report.today.total.label).toBe('TOTAL');
  });

  it('takes an absent or malformed field as absence, never a crash', () => {
    // A report from a server that carries less than this build reads: the
    // rows still land, and the numbers default rather than throwing.
    const report = usageFrom({
      lifetime: {
        by_model: [{ label: 'x' }],
        total: { label: 'TOTAL' },
      },
      pricing_available: 'yes',
    });

    expect(report.lifetime.byModel[0]?.input).toBe(0);
    expect(report.lifetime.byModel[0]?.costUsd).toBe(0);
    expect(report.lifetime.total.label).toBe('TOTAL');
    // A flag the wire did not send as a boolean is unreadable, and unreadable
    // pricing reads as missing rather than as priced.
    expect(report.pricingAvailable).toBe(false);
    expect(report.today.byModel).toEqual([]);
  });

  it('survives a payload that is not a report at all', () => {
    const report = usageFrom(null);
    expect(report.lifetime.total.label).toBe('');
    expect(report.lifetime.total.input).toBe(0);
    expect(report.pricingAvailable).toBe(false);
  });
});
