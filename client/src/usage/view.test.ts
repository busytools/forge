import { describe, expect, it } from 'vitest';

import type { UsageReport, UsageRow } from '../wire/usage';
import {
  formatCost,
  formatTokens,
  headline,
  isEmpty,
  isGpt,
  quickTotals,
  split,
  tableRows,
} from './view';

function row(label: string, over: Partial<UsageRow> = {}): UsageRow {
  return {
    label,
    input: 0,
    cacheWrite1h: 0,
    cacheWrite5m: 0,
    cacheRead: 0,
    output: 0,
    costUsd: 0,
    ...over,
  };
}

function report(over: Partial<UsageReport> = {}): UsageReport {
  const window = {
    byModel: [
      row('claude-opus-5', {
        input: 1_800_000,
        cacheRead: 228_000_000,
        output: 900_000,
        costUsd: 115,
      }),
    ],
    byProject: [
      row('forge', { input: 1_800_000, cacheRead: 228_000_000, output: 900_000, costUsd: 115 }),
    ],
    total: row('TOTAL', {
      input: 1_800_000,
      cacheWrite1h: 12_000_000,
      cacheRead: 228_000_000,
      output: 900_000,
      costUsd: 115,
    }),
  };
  return {
    today: window,
    week: window,
    month: window,
    lifetime: window,
    pricingAvailable: true,
    ...over,
  };
}

describe('the usage figures', () => {
  it('scales tokens the way the terminal does', () => {
    expect(formatTokens(0)).toBe('0');
    expect(formatTokens(950)).toBe('950');
    expect(formatTokens(1_500)).toBe('1.5K');
    expect(formatTokens(1_800_000)).toBe('1.8M');
    expect(formatTokens(680_500_000)).toBe('680.5M');
    expect(formatTokens(1_000_000_000)).toBe('1.0B');
  });

  it('blanks a cost with nothing behind it rather than drawing a zero', () => {
    expect(formatCost(115, true)).toBe('$115.00');
    expect(formatCost(0, true), 'an unpriced row is a dash, not $0.00').toBe('-');
    expect(formatCost(115, false), 'no pricing table means no cost, whatever the number').toBe('-');
  });

  it('summarises the lifetime window', () => {
    const head = headline(report());
    expect(head.tokens).toBe('242.7M');
    expect(head.cost).toBe('$115.00');
    expect(head.projects).toBe(1);
    expect(head.models).toBe(1);
    expect(head.caption).toEqual({ text: 'notional · at API pricing · not a bill', tone: 'dim' });

    const unpriced = headline(report({ pricingAvailable: false }));
    expect(unpriced.cost).toBeNull();
    expect(unpriced.caption).toEqual({ text: 'pricing pending or failed', tone: 'warn' });
  });

  it('splits the lifetime total, the share against the same denominator the terminal uses', () => {
    const parts = split(report());
    expect(parts.input).toBe('1.8M');
    expect(parts.cacheWrite, 'both cache-write fields are one column').toBe('12.0M');
    expect(parts.cacheRead).toBe('228.0M');
    expect(parts.output).toBe('900.0K');
    // 228 / 242.7M total (all five fields) = 93.9 -> integer division floors
    // it.
    expect(parts.cacheReadShare).toBe('93%');
  });

  it('leaves the share absent when nothing was read', () => {
    const quiet = report({
      lifetime: {
        byModel: [],
        byProject: [],
        total: row('TOTAL'),
      },
    });
    expect(split(quiet).cacheReadShare).toBeNull();
  });

  it('orders the quick totals the way the body reads them', () => {
    const totals = quickTotals(report());
    expect(totals.map((total) => total.label)).toEqual(['this month', 'this week', 'today']);
    expect(totals[0]?.cost).toBe('$115.00');
    expect(quickTotals(report({ pricingAvailable: false }))[0]?.cost).toBe('-');
  });

  it('flags a GPT model row as approximate, and never a project', () => {
    expect(isGpt('gpt-5-codex')).toBe(true);
    expect(isGpt('codex-mini')).toBe(true);
    expect(isGpt('claude-opus-5')).toBe(false);

    const withGpt = report({
      lifetime: {
        byModel: [row('gpt-5-codex', { costUsd: 3.6 })],
        byProject: [row('codex-tools', { costUsd: 3.6 })],
        total: row('TOTAL', { costUsd: 3.6 }),
      },
    });
    const model = tableRows(withGpt, 'lifetime', 'model');
    expect(model.body[0]?.approx, 'a GPT model row is flagged in the model grouping').toBe(true);
    expect(model.total.approx, 'and the TOTAL row never is').toBe(false);

    const project = tableRows(withGpt, 'lifetime', 'project');
    expect(project.body[0]?.label).toBe('codex-tools');
    expect(project.body[0]?.approx, 'a project that merely reads GPT-shaped is not a model').toBe(
      false,
    );
  });

  it('draws the rows the chosen window and grouping name, the total beside them', () => {
    const table = tableRows(report(), 'lifetime', 'project');
    expect(table.body.map((entry) => entry.label)).toEqual(['forge']);
    expect(table.total.label).toBe('TOTAL');
    const opus = tableRows(report(), 'lifetime', 'model').body[0];
    expect(opus?.tokens, 'the row carries its five-field sum').toBe('230.7M');
    expect(opus?.cost).toBe('$115.00');
  });

  /** `scratch` and `<synthetic>` name where tokens pooled rather than what
   * produced them, and both are drawn quiet - the TOTAL row names everything
   * and is never one of them. */
  it('marks the pooled labels quiet', () => {
    const pooled = report({
      lifetime: {
        byModel: [row('<synthetic>', { output: 500 })],
        byProject: [row('scratch', { output: 500 })],
        total: row('TOTAL', { output: 500 }),
      },
    });
    expect(tableRows(pooled, 'lifetime', 'project').body.map((entry) => entry.quiet)).toEqual([
      true,
    ]);
    expect(tableRows(pooled, 'lifetime', 'model').body[0]?.quiet).toBe(true);
  });

  it('reads an all-zero lifetime as an empty pool', () => {
    expect(isEmpty(report())).toBe(false);
    const nothing = report({
      today: { byModel: [], byProject: [], total: row('TOTAL') },
      week: { byModel: [], byProject: [], total: row('TOTAL') },
      month: { byModel: [], byProject: [], total: row('TOTAL') },
      lifetime: { byModel: [], byProject: [], total: row('TOTAL') },
    });
    expect(isEmpty(nothing)).toBe(true);
  });
});
