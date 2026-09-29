/**
 * The figures a turn's row draws.
 *
 * They are the view's rather than the record's, which is why the record
 * carries numbers and instants and never a formatted string: a clock baked at
 * write time outlives the zone it was baked in, and a token count rounded for
 * one row is wrong in a body that has room for the whole figure.
 */

/** What a row draws where a figure is absent: a dash, never a zero. */
const MISSING = '-';

/** A span, in the unit it reads best in. */
export function duration(ms: number | null): string {
  if (ms === null) return MISSING;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  const whole = Math.round(ms / 1000);
  return `${Math.floor(whole / 60)}m ${String(whole % 60).padStart(2, '0')}s`;
}

/** A token count as a row draws it: short, because a row has no room. */
export function tokens(count: number): string {
  if (count < 1000) return String(count);
  return `${(count / 1000).toFixed(1)}k`;
}

/** A token count as a body draws it: every digit, grouped. */
export function grouped(count: number): string {
  return count.toLocaleString('en-US');
}

/** Money, as the CLI reports it. */
export function money(usd: number): string {
  return `$${usd.toFixed(2)}`;
}

/**
 * An instant, as the wall clock of whoever is reading.
 *
 * `null` for an instant that does not parse rather than a broken clock: a
 * record read from a transcript carries a stamp written years ago in a zone
 * nobody kept, and `Invalid Date` is not a time.
 */
export function clock(utc: string | null): string | null {
  if (utc === null) return null;
  const at = new Date(utc);
  if (Number.isNaN(at.getTime())) return null;
  const pad = (value: number): string => String(value).padStart(2, '0');
  return `${pad(at.getHours())}:${pad(at.getMinutes())}:${pad(at.getSeconds())}`;
}
