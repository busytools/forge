/**
 * What a turn's metadata row does with its record, in one copy.
 *
 * The row draws under a settled turn and above the box while one runs, and it
 * is the same row: the pinned strip is what the reader watches turn into the
 * finished one. So the rules it applies to the record live here rather than in
 * either component, which is what keeps the two from answering one question
 * two ways.
 */

import type { TurnInfo } from './units';

/**
 * The record with an unattributed usage block dropped.
 *
 * A frame whose counters are all zero is the CLI saying it has nothing to
 * attribute, and a compaction result is that shape; a real zero inside a block
 * that does carry counters is a measurement, and prints as one.
 */
export function attributed(info: TurnInfo): TurnInfo {
  const nothing =
    (info.input_tokens ?? 0) === 0 &&
    (info.output_tokens ?? 0) === 0 &&
    (info.cache_read_tokens ?? 0) === 0 &&
    (info.cache_written_tokens ?? 0) === 0;
  return nothing
    ? {
        ...info,
        input_tokens: null,
        output_tokens: null,
        cache_read_tokens: null,
        cache_written_tokens: null,
      }
    : info;
}

/**
 * The share of this turn's input served from the cache, over every input-side
 * counter.
 *
 * `null` when the record is missing any of the three counters: a missing
 * counter is an absent measurement, not a zero, and a share computed with it
 * read as zero is a figure the wire never stated. An all-zero block has a
 * zero denominator and so no share either; a present read of zero among
 * counters that add up is a measurement and divides to `0`.
 */
export function cached(info: TurnInfo): number | null {
  const read = info.cache_read_tokens;
  const input = info.input_tokens;
  const written = info.cache_written_tokens;
  if (read === null || input === null || written === null) return null;
  const total = read + input + written;
  return total === 0 ? null : Math.floor((read * 100) / total);
}

/**
 * The span a running record measures at `now`: its frames' own span plus the
 * wait since the last of them.
 *
 * The two terms cancel to `now - turn start` while a frame's span and its
 * stamp agree - which is what lets a reader watch the clock at all: the wait
 * is the part the local tick drives, and the frame lands as a correction.
 */
export function runningMs(info: TurnInfo, now: number): number {
  const since = info.ended_at_utc === null ? 0 : now - Date.parse(info.ended_at_utc);
  const waited = Number.isFinite(since) && since > 0 ? since : 0;
  return (info.duration_ms ?? 0) + waited;
}
