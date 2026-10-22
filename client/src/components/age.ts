/**
 * How long since a stamp, in the words the board and the review share:
 * seconds under a minute, minutes under an hour, hours until `daysFrom`,
 * and days at or past it.
 *
 * **`daysFrom` is the caller's**, because the two surfaces word the past
 * differently: the review's picker reads a day-old stamp as hours
 * ("26h ago", supplied at two days) while the board keeps counting in hours
 * past that. A caller that wants days passes its own threshold; one that
 * does not gets the board's older wording unchanged.
 */
export function ago(
  seconds: number,
  nowSeconds: number,
  daysFrom = Number.POSITIVE_INFINITY,
): string {
  const d = Math.max(0, nowSeconds - seconds);
  if (d >= daysFrom) return `${Math.floor(d / 86_400)}d`;
  if (d >= 3_600) return `${Math.floor(d / 3_600)}h`;
  if (d >= 60) return `${Math.floor(d / 60)}m`;
  return `${d}s`;
}
