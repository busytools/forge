/**
 * The window's digest strip: one line per count, each opening onto the
 * items it counts.
 *
 * **Pure, and tested on its own**: the component draws what this returns
 * and decides nothing about the digest itself, so the strip's shape is a
 * fact a test can hold rather than a thing only a browser shows.
 */

import type { AttentionView } from './window';

/** One row behind a digest count. */
export interface DigestItem {
  id: string | null;
  subject: string;
  /** What happened to it, in the board's own words. */
  word: string;
  at: number;
}

/** One line of the strip: a count, its own label, and what it counts. */
export interface DigestLine {
  key: 'merged' | 'moved' | 'landed' | 'expired' | 'sampled';
  label: string;
  /** The count itself, for a line drawn as a number. */
  count: number;
  /** The count in words: a plain number, or the sample's `3 of 8 actions`. */
  countLabel: string;
  items: DigestItem[];
}

/**
 * The strip's five lines, in the order the research ranks them: what
 * landed, what moved, what expired, what merged, and the sample of work
 * nobody escalated. Every line is drawn even at zero: a fact about the
 * window, not an absence.
 */
export function digestLines(attention: AttentionView): DigestLine[] {
  const sample = attention.sample;
  return [
    {
      key: 'merged',
      label: 'merged',
      count: attention.merged,
      countLabel: String(attention.merged),
      items: attention.items.merged,
    },
    {
      key: 'moved',
      label: 'moved',
      count: attention.moved,
      countLabel: String(attention.moved),
      items: attention.items.moved,
    },
    {
      key: 'landed',
      label: 'landed',
      count: attention.landed,
      countLabel: String(attention.landed),
      items: attention.items.landed,
    },
    {
      key: 'expired',
      label: 'expired',
      count: attention.expired,
      countLabel: String(attention.expired),
      items: attention.items.expired,
    },
    {
      key: 'sampled',
      label: 'sampled',
      count: sample.total,
      countLabel: `${sample.sampled} of ${sample.total} agent actions`,
      items: sample.rows.map((row) => ({
        id: null,
        subject: row.seat,
        word: row.detail === '' ? row.tool : `${row.tool} ${row.detail}`,
        at: row.at,
      })),
    },
  ];
}

/** The sample's words inside the all-clear sentence. */
export function sampleClaim(attention: AttentionView): string {
  return `sampled ${attention.sample.sampled} of ${attention.sample.total} agent actions`;
}
