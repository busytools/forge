/**
 * A rate-limit window's line, ported arm for arm from the terminal's
 * `app/events/rate_limit.rs` (`format_rate_limit_summary` and the notice key),
 * so both views word one window the same way.
 *
 * The reads are the wire's own names: camelCase inside the frame's
 * `rate_limit_info` (`resetsAt`, `rateLimitType`, `isUsingOverage`), and
 * nothing links them to the Rust types - a rename upstream would stop
 * matching in silence.
 */

type Info = Record<string, unknown>;

const EXTRA_USAGE_REQUIRED =
  'Extra usage credit is required to continue. Use /extra-usage to enable it, /model to switch models, or wait for the rate-limit window to reset.';

/** A wire string field that carries something, or null. */
function line(info: Info, key: string): string | null {
  const held = info[key];
  return typeof held === 'string' && held !== '' ? held : null;
}

/** A wire number field, or null for absent and non-finite alike. */
function figure(info: Info, key: string): number | null {
  const held = info[key];
  return typeof held === 'number' && Number.isFinite(held) ? held : null;
}

/** A wire boolean field, or null. */
function flag(info: Info, key: string): boolean | null {
  const held = info[key];
  return typeof held === 'boolean' ? held : null;
}

/** A consumed fraction as the terminal prints it: `{:.0}`, ties to even. */
function percent(utilization: number): number {
  const value = utilization * 100;
  const floor = Math.floor(value);
  const rest = value - floor;
  if (rest > 0.5) return floor + 1;
  if (rest < 0.5) return floor;
  return floor % 2 === 0 ? floor : floor + 1;
}

function formatRateLimitType(raw: string): string {
  switch (raw) {
    case 'five_hour':
      return '5-hour';
    case 'daily':
      return 'daily';
    case 'minute':
      return 'per-minute';
    case 'seven_day':
      return '7-day';
    case 'seven_day_opus':
      return '7-day Opus';
    case 'seven_day_sonnet':
      return '7-day Sonnet';
    case 'overage':
      return 'overage';
    default:
      return raw;
  }
}

/**
 * An epoch timestamp as a countdown and UTC wall-clock: "4h 23m at 14:30 UTC".
 *
 * A negative or non-finite value reads "now" rather than throwing, the guard
 * the terminal keeps on the same input (clock skew, a CLI bug).
 */
export function formatResetsAt(epochSeconds: number): string {
  if (!Number.isFinite(epochSeconds) || epochSeconds < 0) return 'now';
  const remaining = Math.floor((epochSeconds * 1000 - Date.now()) / 1000);
  const hours = Math.floor(remaining / 3600);
  const minutes = Math.floor((remaining % 3600) / 60);
  const countdown =
    remaining < 0
      ? 'now'
      : remaining < 60
        ? '< 1 minute'
        : hours > 0
          ? `${hours}h ${minutes}m`
          : `${minutes}m`;
  const held = Math.floor(epochSeconds);
  const hh = String(Math.floor((held % 86400) / 3600)).padStart(2, '0');
  const mm = String(Math.floor((held % 3600) / 60)).padStart(2, '0');
  return `${countdown} at ${hh}:${mm} UTC`;
}

/** True when the report still names which window it is: the loud words need one. */
function hasWindowContext(info: Info): boolean {
  return (
    figure(info, 'utilization') !== null ||
    line(info, 'rateLimitType') !== null ||
    figure(info, 'resetsAt') !== null
  );
}

/**
 * A window past its threshold with no overage actually consumed: the loud
 * percentage over-states it, so only the reset - the actionable bit - is kept.
 */
function nearThresholdWithoutOverage(info: Info): boolean {
  const surpassed = figure(info, 'surpassedThreshold');
  return (
    line(info, 'status') === 'allowed_warning' &&
    flag(info, 'isUsingOverage') === false &&
    surpassed !== null &&
    surpassed > 0
  );
}

/** The window's line, as the terminal words it. */
export function formatRateLimitSummary(info: Info): string {
  if (
    line(info, 'status') === 'rejected' &&
    !hasWindowContext(info) &&
    flag(info, 'isUsingOverage') === false &&
    line(info, 'overageDisabledReason') === 'org_level_disabled'
  ) {
    return EXTRA_USAGE_REQUIRED;
  }

  if (nearThresholdWithoutOverage(info)) {
    let softened = 'Near rate-limit threshold.';
    const resetsAt = figure(info, 'resetsAt');
    if (resetsAt !== null) softened += ` Resets in ${formatResetsAt(resetsAt)}.`;
    return softened;
  }

  const rejected = line(info, 'status') === 'rejected';
  const intro = rejected ? 'Rate limit reached' : 'Approaching rate limit';
  const utilization = figure(info, 'utilization');
  const type = line(info, 'rateLimitType');
  const usage =
    utilization !== null && type !== null
      ? `you've used ${percent(utilization)}% of your ${formatRateLimitType(type)} rate limit`
      : utilization !== null
        ? `you've used ${percent(utilization)}% of your rate limit`
        : type !== null
          ? `you've hit your ${formatRateLimitType(type)} rate limit`
          : "you've hit your rate limit";

  let message = `${intro}, ${usage}.`;

  const usingOverage = flag(info, 'isUsingOverage');
  if (rejected) {
    if (usingOverage === true) message += ' You are using your overage allowance.';
  } else if (usingOverage === false || info['overageStatus'] !== undefined) {
    message += ' You can continue using your overage allowance.';
  }

  const resetsAt = figure(info, 'resetsAt');
  if (resetsAt !== null) message += ` Resets in ${formatResetsAt(resetsAt)}.`;

  return message;
}

/**
 * The notice key for one incident: the window's type and its reset bucket,
 * so a later frame in the same window rewrites the line and a new window
 * opens one of its own. The status is deliberately not in here - a window
 * escalating from warning to rejected updates the line it already drew.
 */
export function rateLimitNoticeKey(info: Info): string {
  const type = line(info, 'rateLimitType') ?? 'any';
  const resetsAt = figure(info, 'resetsAt');
  const bucket = resetsAt === null ? 'none' : String(Math.floor(Math.max(0, resetsAt)));
  return `rate-limit:${type}:${bucket}`;
}
