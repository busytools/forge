/**
 * How long since a stamp, in the words the board and the review share:
 * seconds under a minute, minutes under an hour, hours under two days, days
 * above that - so a stamp from yesterday reads "26h ago" rather than "1d".
 */
export function ago(seconds: number, nowSeconds: number): string {
  const d = Math.max(0, nowSeconds - seconds);
  if (d >= 172_800) return `${Math.floor(d / 86_400)}d`;
  if (d >= 3_600) return `${Math.floor(d / 3_600)}h`;
  if (d >= 60) return `${Math.floor(d / 60)}m`;
  return `${d}s`;
}
