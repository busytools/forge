/**
 * What a turn's metadata row does with its record, in one copy.
 *
 * The row draws under a settled turn and above the box while one runs, and it
 * is the same row: the pinned strip is what the reader watches turn into the
 * finished one. So the rules it applies to the record live here rather than in
 * either component, which is what keeps the two from answering one question
 * two ways.
 */

import { duration } from './numbers';
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
 * `null` when the record carries no cache read at all, which is a turn that
 * never touched the cache rather than one that missed it entirely.
 */
export function cached(info: TurnInfo): number | null {
  const read = info.cache_read_tokens;
  if (read === null) return null;
  const total = read + (info.input_tokens ?? 0) + (info.cache_written_tokens ?? 0);
  return total === 0 ? null : Math.floor((read * 100) / total);
}

/**
 * The clock the row leads with: the span an unfinished turn's frames measure,
 * plus the wait since its last one, and a settled turn's own clock.
 */
export function elapsed(info: TurnInfo, now: number): string {
  if (!info.running) return duration(info.duration_ms);
  const since = info.ended_at_utc === null ? 0 : now - Date.parse(info.ended_at_utc);
  const waited = Number.isFinite(since) && since > 0 ? since : 0;
  return duration((info.duration_ms ?? 0) + waited);
}
