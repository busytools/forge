/**
 * A device's own name for the settings list.
 *
 * **The platform plus this enrolment's own serial** is what tells two devices
 * of one platform apart, because a page has no hostname on any client. On the
 * web the browser is named as well - "Chrome on macOS (3f2a9c1b)" - since two
 * browsers on one machine are two devices and the row should say which. The
 * shell clients keep their name: the phone's naming is its own piece.
 */

import { isTauri } from '@tauri-apps/api/core';

/** The browser as a person names it, from the brands it reports, else its UA. */
function browserName(): string {
  const data = (
    globalThis.navigator as {
      userAgentData?: { brands?: { brand?: string }[] };
    }
  ).userAgentData;
  const brands = (data?.brands ?? [])
    .map((held) => held.brand)
    .filter((brand): brand is string => typeof brand === 'string');
  // **The most specific brand wins.** Brave and Edge advertise Chromium too,
  // and a row reading "Chromium" would hide which browser holds the passkey.
  const known: [string, string][] = [
    ['Brave', 'Brave'],
    ['Microsoft Edge', 'Edge'],
    ['Google Chrome', 'Chrome'],
    ['Opera', 'Opera'],
    ['Vivaldi', 'Vivaldi'],
    ['Firefox', 'Firefox'],
    ['Chromium', 'Chromium'],
  ];
  for (const [brand, name] of known) {
    if (brands.includes(brand)) return name;
  }
  const agent = globalThis.navigator.userAgent;
  if (agent.includes('Edg/')) return 'Edge';
  if (agent.includes('OPR/')) return 'Opera';
  if (agent.includes('Brave')) return 'Brave';
  // Chrome's UA carries `Safari/` too, so the specific checks lead.
  if (agent.includes('Chrome/') || agent.includes('Chromium/')) return 'Chrome';
  if (agent.includes('Firefox/')) return 'Firefox';
  if (agent.includes('Safari/')) return 'Safari';
  return 'browser';
}

/** The platform as a person names it, from the client hint, else the UA. */
function platformName(): string {
  const data = (globalThis.navigator as { userAgentData?: { platform?: string } }).userAgentData;
  const named = data?.platform;
  if (named !== undefined && named !== '') return named;
  switch (globalThis.navigator.platform) {
    case 'MacIntel':
    case 'Macintosh':
      return 'macOS';
    case 'Win32':
    case 'Win64':
      return 'Windows';
    case 'iPhone':
    case 'iPad':
      return 'iOS';
    case 'Linux x86_64':
    case 'Linux armv8l':
    case 'Linux aarch64':
      return 'Linux';
    default: {
      const legacy = globalThis.navigator.platform;
      return legacy === '' ? 'this device' : legacy;
    }
  }
}

/** The name this device enrols under. */
export function deviceName(enrolment: string): string {
  const serial = enrolment.slice(0, 8);
  if (isTauri()) {
    const data = (globalThis.navigator as { userAgentData?: { platform?: string } }).userAgentData;
    const platform = data?.platform ?? globalThis.navigator.platform;
    const name = platform === undefined || platform === '' ? 'this device' : platform;
    return `${name} (${serial})`;
  }
  return `${browserName()} on ${platformName()} (${serial})`;
}
