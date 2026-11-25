// @vitest-environment jsdom
/**
 * This client's own gate state, as the settings card reads it: unavailable
 * with the platform's reason, enrolled with the gesture it answers, or ready
 * to enrol.
 */
import { beforeEach, describe, expect, it, vi } from 'vitest';

const gate = vi.hoisted(() => ({ availability: vi.fn(), enrolled: vi.fn() }));
vi.mock('./index', () => ({ availability: gate.availability, enrolled: gate.enrolled }));

import { readDeviceGate } from './device';

beforeEach(() => {
  gate.availability.mockReset();
  gate.enrolled.mockReset();
});

describe("this client's gate state", () => {
  it('reports an unavailable gate with its reason', async () => {
    gate.availability.mockResolvedValue({
      available: false,
      biometric: false,
      reason: 'this browser has no PRF; use the password',
    });

    expect(await readDeviceGate()).toEqual({
      outcome: 'unavailable',
      reason: 'this browser has no PRF; use the password',
    });
    expect(gate.enrolled, 'an unavailable gate is not asked for a record').not.toHaveBeenCalled();
  });

  it('reports an enrolled gate with the gesture it answers', async () => {
    gate.availability.mockResolvedValue({ available: true, biometric: true });
    gate.enrolled.mockResolvedValue(true);

    expect(await readDeviceGate()).toEqual({ outcome: 'enrolled', biometric: true });
    expect(gate.enrolled).toHaveBeenCalledWith('vault');
  });

  it('reports a ready gate when this client holds nothing', async () => {
    gate.availability.mockResolvedValue({ available: true, biometric: false });
    gate.enrolled.mockResolvedValue(false);

    expect(await readDeviceGate()).toEqual({ outcome: 'ready', biometric: false });
  });

  it('falls back to its own words when the gate gives no reason', async () => {
    gate.availability.mockResolvedValue({ available: false, biometric: false });

    expect(await readDeviceGate()).toEqual({
      outcome: 'unavailable',
      reason: 'this device has no authenticator',
    });
  });
});
