// @vitest-environment jsdom
/**
 * This client's own gate state, as the settings card reads it: unavailable
 * with the platform's reason, enrolled with the gesture it answers, or ready
 * to enrol.
 *
 * **"Enrolled" is the record this client minted**, not a keychain read: on
 * macOS asking the keychain is itself a gesture, and a skipped read of a
 * protected item answers not-found, so the keychain cannot answer this
 * without either a prompt or a lie.
 */
import { beforeEach, describe, expect, it, vi } from 'vitest';

const gate = vi.hoisted(() => ({ availability: vi.fn() }));
vi.mock('./index', () => ({ availability: gate.availability }));
const record = vi.hoisted(() => ({ enrolmentIdIfMinted: vi.fn() }));
vi.mock('./vault-enrolment', () => ({ enrolmentIdIfMinted: record.enrolmentIdIfMinted }));

import { readDeviceGate } from './device';

beforeEach(() => {
  gate.availability.mockReset();
  record.enrolmentIdIfMinted.mockReset();
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
    expect(
      record.enrolmentIdIfMinted,
      'an unavailable gate is not asked for a record',
    ).not.toHaveBeenCalled();
  });

  it('reports an enrolled gate with the gesture it answers', async () => {
    gate.availability.mockResolvedValue({ available: true, biometric: true });
    record.enrolmentIdIfMinted.mockReturnValue('device-1');

    expect(await readDeviceGate()).toEqual({ outcome: 'enrolled', biometric: true });
    expect(record.enrolmentIdIfMinted).toHaveBeenCalled();
  });

  it('reports a ready gate when this client minted no record', async () => {
    gate.availability.mockResolvedValue({ available: true, biometric: false });
    record.enrolmentIdIfMinted.mockReturnValue(null);

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
