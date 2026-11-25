// @vitest-environment jsdom
/**
 * A device's name for the enrolment list: what tells two devices of one
 * platform apart, and on the web which browser it is.
 */
import { beforeEach, describe, expect, it, vi } from 'vitest';

const state = vi.hoisted(() => ({ tauri: false }));
vi.mock('@tauri-apps/api/core', () => ({ isTauri: () => state.tauri }));

import { deviceName } from './name';

const ID = '3f2a9c1b-8d4e-4f0a-9b7c-1d2e3f4a5b6c';

function withNavigator(held: {
  userAgentData?: unknown;
  platform?: string;
  userAgent?: string;
}): void {
  Object.defineProperty(globalThis.navigator, 'userAgentData', {
    configurable: true,
    value: held.userAgentData,
  });
  Object.defineProperty(globalThis.navigator, 'platform', {
    configurable: true,
    value: held.platform ?? '',
  });
  Object.defineProperty(globalThis.navigator, 'userAgent', {
    configurable: true,
    value: held.userAgent ?? '',
  });
}

beforeEach(() => {
  state.tauri = false;
  withNavigator({});
});

describe('a web device name', () => {
  it('names the browser and the platform, with the serial the list needs', () => {
    withNavigator({
      userAgentData: {
        platform: 'macOS',
        brands: [{ brand: 'Chromium' }, { brand: 'Google Chrome' }, { brand: 'Not_A Brand' }],
      },
    });

    expect(deviceName(ID)).toBe('Chrome on macOS (3f2a9c1b)');
  });

  /// **The most specific brand wins.** Brave advertises Chromium too, and a
  /// row reading "Chromium" would hide which browser holds the passkey.
  it('prefers the specific brand over Chromium', () => {
    withNavigator({
      userAgentData: {
        platform: 'macOS',
        brands: [{ brand: 'Chromium' }, { brand: 'Brave' }, { brand: 'Not_A Brand' }],
      },
    });

    expect(deviceName(ID)).toBe('Brave on macOS (3f2a9c1b)');
  });

  it('reads the user agent when the browser reports no brands', () => {
    withNavigator({
      platform: 'MacIntel',
      userAgent:
        'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Version/18.0 Safari/605.1.15',
    });

    expect(deviceName(ID)).toBe('Safari on macOS (3f2a9c1b)');
  });

  it('names an Android browser from its platform', () => {
    withNavigator({
      userAgentData: { platform: 'Android', brands: [{ brand: 'Google Chrome' }] },
    });

    expect(deviceName(ID)).toBe('Chrome on Android (3f2a9c1b)');
  });

  it('falls back when the browser says nothing', () => {
    expect(deviceName(ID)).toBe('browser on this device (3f2a9c1b)');
  });
});

describe('a shell device name', () => {
  /// **The shell clients keep their name.** The phone's naming is its own
  /// piece; this cut changes only what a browser sends.
  it('keeps the platform and the serial, with no browser', () => {
    state.tauri = true;
    withNavigator({ userAgentData: { platform: 'macOS' }, platform: 'MacIntel' });

    const name = deviceName(ID);

    expect(name).toBe('macOS (3f2a9c1b)');
    expect(name).not.toContain(' on ');
  });

  it('keeps the legacy platform name when there is no user-agent client hint', () => {
    state.tauri = true;
    withNavigator({ platform: 'MacIntel' });

    expect(deviceName(ID)).toBe('MacIntel (3f2a9c1b)');
  });
});
