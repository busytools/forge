/**
 * The usage page's derivations: everything it draws, computed from the report
 * and nothing else.
 *
 * The terminal's `/usage` overlay is the reference for every rule here - the
 * unit scales, the dash for a cost with no price behind it, the share
 * computed over the whole token sum, the GPT flag being about a MODEL row and
 * never a project - so the page reads as the overlay read, which is the one
 * thing that has drawn this report before.
 *
 * Pure, so a test can build a report by hand.
 */

import type { UsageReport, UsageRow, WindowUsage } from '../wire/usage';

export type Grouping = 'project' | 'model';
export type WindowKey = 'today' | 'week' | 'month' | 'lifetime';

/** The four windows, in the order the selector offers them. */
export const WINDOWS: readonly { key: WindowKey; label: string }[] = [
  { key: 'today', label: 'today' },
  { key: 'week', label: 'week' },
  { key: 'month', label: 'month' },
  { key: 'lifetime', label: 'lifetime' },
];

export function windowOf(report: UsageReport, key: WindowKey): WindowUsage {
  return report[key];
}

/** The five fields a row's token total sums. */
export function rowTokens(row: UsageRow): number {
  return row.input + row.cacheWrite1h + row.cacheWrite5m + row.cacheRead + row.output;
}

/**
 * A token count in the unit it reads best in, the terminal's scales and
 * truncation (one decimal, never rounded up).
 */
export function formatTokens(n: number): string {
  const B = 1_000_000_000;
  const M = 1_000_000;
  const K = 1_000;
  if (n >= B) return `${Math.floor(n / B)}.${Math.floor(((n % B) * 10) / B)}B`;
  if (n >= M) return `${Math.floor(n / M)}.${Math.floor(((n % M) * 10) / M)}M`;
  if (n >= K) return `${Math.floor(n / K)}.${Math.floor(((n % K) * 10) / K)}K`;
  return String(n);
}

/**
 * A cost as a table cell or a total: `$x.xx` when a price stood behind it,
 * a dash when it did not. A dash rather than `$0.00`, because a zero is a
 * reading and no price is not one.
 */
export function formatCost(costUsd: number, pricingAvailable: boolean): string {
  if (!pricingAvailable || costUsd <= 0) return '-';
  return `$${costUsd.toFixed(2)}`;
}

/** The lifetime headline: the figures and the caption that qualifies them. */
export interface Headline {
  tokens: string;
  /** `null` when no pricing table stood behind the figure. */
  cost: string | null;
  projects: number;
  models: number;
  caption: { text: string; tone: 'dim' | 'warn' };
}

export function headline(report: UsageReport): Headline {
  const life = report.lifetime;
  return {
    tokens: formatTokens(rowTokens(life.total)),
    cost: report.pricingAvailable ? `$${life.total.costUsd.toFixed(2)}` : null,
    projects: life.byProject.length,
    models: life.byModel.length,
    caption: report.pricingAvailable
      ? { text: 'notional · at API pricing · not a bill', tone: 'dim' }
      : { text: 'pricing pending or failed', tone: 'warn' },
  };
}

/** The lifetime split, the cache share included. */
export interface Split {
  input: string;
  cacheWrite: string;
  cacheRead: string;
  /** `null` when the lifetime total is zero - there is no share of nothing. */
  cacheReadShare: string | null;
  output: string;
}

export function split(report: UsageReport): Split {
  const life = report.lifetime.total;
  const tokens = rowTokens(life);
  return {
    input: formatTokens(life.input),
    cacheWrite: formatTokens(life.cacheWrite1h + life.cacheWrite5m),
    cacheRead: formatTokens(life.cacheRead),
    cacheReadShare: tokens === 0 ? null : `${Math.floor((life.cacheRead * 100) / tokens)}%`,
    output: formatTokens(life.output),
  };
}

/** One quick total beside the headline. */
export interface QuickTotal {
  label: string;
  tokens: string;
  cost: string;
}

/** The three rolling totals, in the order the summary row reads them. */
export function quickTotals(report: UsageReport): QuickTotal[] {
  return [
    { label: 'this month', window: report.month },
    { label: 'this week', window: report.week },
    { label: 'today', window: report.today },
  ].map(({ label, window }) => ({
    label,
    tokens: formatTokens(rowTokens(window.total)),
    cost: formatCost(window.total.costUsd, report.pricingAvailable),
  }));
}

/** The terminal's own reading of the model names it calls approximate. */
export function isGpt(label: string): boolean {
  return label.startsWith('gpt') || label.includes('codex');
}

/** One table row, resolved for the markup. */
export interface TableRow {
  label: string;
  input: string;
  cacheWrite: string;
  cacheRead: string;
  output: string;
  tokens: string;
  cost: string;
  /** A model row whose price is an approximation; never a project or TOTAL. */
  approx: boolean;
  /** A pooled label rather than a real one - the terminal draws these quiet. */
  quiet: boolean;
}

/** The labels that name where tokens pooled rather than what produced them. */
const QUIET_LABELS = ['scratch', '<synthetic>'];

/** The rows one window's table draws: the grouping's rows, and the pinned total. */
export interface UsageTable {
  body: TableRow[];
  total: TableRow;
}

/**
 * The rows the table draws: the chosen window's grouping, the total pinned
 * below them.
 *
 * The GPT flag rides the MODEL grouping only - a project named after a model
 * is not an approximated model - and never the TOTAL row.
 */
export function tableRows(report: UsageReport, key: WindowKey, grouping: Grouping): UsageTable {
  const window = windowOf(report, key);
  const rows = grouping === 'model' ? window.byModel : window.byProject;
  const body = rows.map((row) => ({
    label: row.label,
    input: formatTokens(row.input),
    cacheWrite: formatTokens(row.cacheWrite1h + row.cacheWrite5m),
    cacheRead: formatTokens(row.cacheRead),
    output: formatTokens(row.output),
    tokens: formatTokens(rowTokens(row)),
    cost: formatCost(row.costUsd, report.pricingAvailable),
    approx: grouping === 'model' && isGpt(row.label),
    quiet: QUIET_LABELS.includes(row.label),
  }));
  const total = window.total;
  return {
    body,
    total: {
      label: total.label === '' ? 'TOTAL' : total.label,
      input: formatTokens(total.input),
      cacheWrite: formatTokens(total.cacheWrite1h + total.cacheWrite5m),
      cacheRead: formatTokens(total.cacheRead),
      output: formatTokens(total.output),
      tokens: formatTokens(rowTokens(total)),
      cost: formatCost(total.costUsd, report.pricingAvailable),
      approx: false,
      quiet: false,
    },
  };
}

/** Whether the pool holds nothing: an all-zero lifetime is no usage yet. */
export function isEmpty(report: UsageReport): boolean {
  return rowTokens(report.lifetime.total) === 0;
}
