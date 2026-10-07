/**
 * A release version as three integers, or `null` for anything else - a
 * prerelease suffix, a fourth segment, junk. Every segment must parse: a
 * compare that dropped the ones that did not would read `x.1.0.116` as
 * `1.0.116` and offer it. The phone's Kotlin compare runs the same rule.
 *
 * **A build stamp after `+` is dropped before the parse**, which is
 * semver's own rule for build metadata and the client's own version's
 * shape now: a compare that refused the stamp would stop offering updates
 * to every stamped build.
 */
export function versionParts(version: string): number[] | null {
  const core = version.trim().replace(/^v/, '').split('+')[0] ?? '';
  const segments = core.split('.');
  if (segments.length !== 3) return null;
  const numbers: number[] = [];
  for (const segment of segments) {
    if (!/^\d+$/.test(segment)) return null;
    numbers.push(Number(segment));
  }
  return numbers;
}

/** Strictly newer, and only when both sides parse; anything else is no. */
export function isNewer(candidate: string, current: string): boolean {
  const next = versionParts(candidate);
  const held = versionParts(current);
  if (next === null || held === null) return false;
  for (const [index, part] of next.entries()) {
    const other = held[index];
    if (other === undefined) return false;
    if (part !== other) return part > other;
  }
  return false;
}
