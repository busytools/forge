/**
 * The cause a silent capture owes, mapped from what the platform reported.
 *
 * Pure by construction: the readers live in the shell, so what a page can
 * decide is which of the reported facts to name and in what order. This is
 * that decision and nothing else, and the rig is what proves the readers'
 * half of it.
 */

import { describe, expect, it } from 'vitest';

import { silenceCause, type MicSilence } from './mic';

/** The platform's answer, with whatever this case is about set. */
function reported(over: Partial<MicSilence> = {}): MicSilence {
  return { muted: false, silenced: false, ...over };
}

describe('the cause a silent capture owes', () => {
  it('says nothing when the platform reports nothing wrong', () => {
    expect(
      silenceCause(reported()),
      'an answer with nothing in it must not invent a cause',
    ).toBeNull();
  });

  it('says nothing when there is no answer at all', () => {
    expect(silenceCause(null), 'a platform with no reader names no cause').toBeNull();
  });

  it("names the phone silencing this app's microphone", () => {
    expect(silenceCause(reported({ silenced: true }))).toContain('silencing');
  });

  it('names the device mute', () => {
    expect(silenceCause(reported({ muted: true }))).toContain('muted');
  });

  it('names every cause it was told, the silencing first', () => {
    const line = silenceCause(reported({ silenced: true, muted: true }));
    expect(line).toContain('silencing');
    expect(line).toContain('muted');
    expect(
      line?.indexOf('silencing'),
      'the setting the reader can change leads, ahead of the device mute',
    ).toBeLessThan(line?.indexOf('muted') ?? 0);
  });
});
