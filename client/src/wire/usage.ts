/**
 * The usage report, as `crates/forge-primitives/src/token_usage.rs` writes it
 * and `Subject::Usage` answers with.
 *
 * The read is the whole page: four rolling windows, each carrying its rows
 * grouped by project and by model plus the total, and whether a pricing table
 * stood behind the costs. The subject hears no updates - the pool changes as
 * transcripts are written and nothing in the stream says so - so a page asks
 * again, and the narrowing here is the only place a wire value becomes a
 * number this client trusts.
 */

/** One labelled row's five token fields and its notional cost. */
export interface UsageRow {
  label: string;
  input: number;
  cacheWrite1h: number;
  cacheWrite5m: number;
  cacheRead: number;
  output: number;
  costUsd: number;
}

/** One window's rows and its total. */
export interface WindowUsage {
  byModel: UsageRow[];
  byProject: UsageRow[];
  total: UsageRow;
}

/**
 * The four windows, plus whether the costs mean anything.
 *
 * `pricingAvailable: false` means every cost is a placeholder, so a page
 * blanks costs rather than drawing a misleading `$0.00`.
 */
export interface UsageReport {
  today: WindowUsage;
  week: WindowUsage;
  month: WindowUsage;
  lifetime: WindowUsage;
  pricingAvailable: boolean;
}

function record(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === 'object' ? (value as Record<string, unknown>) : {};
}

function num(value: unknown): number {
  return typeof value === 'number' && Number.isFinite(value) ? value : 0;
}

function rowFrom(value: unknown): UsageRow {
  const row = record(value);
  return {
    label: typeof row['label'] === 'string' ? row['label'] : '',
    input: num(row['input']),
    cacheWrite1h: num(row['cache_write_1h']),
    cacheWrite5m: num(row['cache_write_5m']),
    cacheRead: num(row['cache_read']),
    output: num(row['output']),
    costUsd: num(row['cost_usd']),
  };
}

function rowsFrom(value: unknown): UsageRow[] {
  return Array.isArray(value) ? value.map(rowFrom) : [];
}

function windowFrom(value: unknown): WindowUsage {
  const window = record(value);
  return {
    byModel: rowsFrom(window['by_model']),
    byProject: rowsFrom(window['by_project']),
    total: rowFrom(window['total']),
  };
}

/** The report, narrowed field by field: an unreadable value is absence. */
export function usageFrom(value: unknown): UsageReport {
  const report = record(value);
  return {
    today: windowFrom(report['today']),
    week: windowFrom(report['week']),
    month: windowFrom(report['month']),
    lifetime: windowFrom(report['lifetime']),
    pricingAvailable: report['pricing_available'] === true,
  };
}
