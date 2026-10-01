/**
 * The dictation panel's own reads: the axes and their labels, the value in
 * force on each, the key hint, and what a chip asks the core for.
 *
 * Pure, so the panel's markup stays about drawing. The vocabulary is the
 * terminal's own (`crates/forge-tui/src/app/dictate_picker.rs`); the form is
 * the mockup's, which is chips rather than the terminal's rows.
 *
 * **Named with the suffix because the row's own suite is `Dictation.test.ts`**,
 * and on a case-insensitive filesystem `dictation.test.ts` is that file - a
 * second suite written under the lowercase name silently replaces it.
 */

import { describe, expect, it } from 'vitest';

import {
  DESTINATION,
  devicePick,
  inForce,
  keyHint,
  MODES,
  OVERRIDE,
  pickUpdate,
  STRUCTURE,
  VOICE,
} from './dictation';

describe('the axes', () => {
  it('carries the terminal vocabulary, with its own labels', () => {
    expect(VOICE.map((option) => option.value)).toEqual([
      'casual',
      'semi_casual',
      'semi_formal',
      'formal',
    ]);
    expect(VOICE.map((option) => option.label)).toContain('semi-formal');
    expect(STRUCTURE.map((option) => option.label)).toEqual(['prose', 'may bullet a list']);
    expect(DESTINATION.map((option) => option.label)).toEqual(['plain text', 'email layout']);
    expect(MODES.map((option) => option.value)).toEqual(['auto', 'toggle', 'hold']);
  });

  it('reads the value in force per axis: the session own, or the crate default', () => {
    expect(
      inForce({ styling: 'casual', structure: null, context: 'email' }),
      'the session set two of the three',
    ).toEqual({ styling: 'casual', structure: 'prose', context: 'email' });

    expect(
      inForce({ styling: null, structure: null, context: null }),
      'and nothing is the crate defaults, which are what a take would use',
    ).toEqual({ styling: 'semi_formal', structure: 'prose', context: 'general' });
  });

  it('asks for one axis at a time, in the core own vocabulary', () => {
    expect(pickUpdate('voice', 'casual')).toEqual({ styling: 'casual' });
    expect(pickUpdate('structure', 'lists')).toEqual({ structure: 'lists' });
    expect(pickUpdate('destination', 'email')).toEqual({ context: 'email' });
    expect(OVERRIDE, 'and the wire name is the Rust variant, not the panel label').toEqual({
      voice: 'styling',
      structure: 'structure',
      destination: 'context',
    });
  });
});

describe('the key hint', () => {
  it('names the key this platform delivers', () => {
    expect(keyHint('right_cmd', true)).toBe('\u{2318} right to talk');
    expect(keyHint('left_cmd', true)).toBe('\u{2318} left to talk');
    expect(keyHint('right_cmd', false), 'where there is no Cmd key').toBe('Ctrl right to talk');
  });

  it('advertises nothing when the binding is off', () => {
    // The config allows it, and a hint naming a key that will never fire the
    // take is worse than no hint at all.
    expect(keyHint('off', true)).toBeNull();
    expect(keyHint('off', false)).toBeNull();
  });
});

describe('the device pick', () => {
  it('reads the pick the home carries, and the pin standing as nothing picked', () => {
    expect(devicePick({ device: 'mic-2' })).toEqual({ device: 'mic-2' });
    expect(devicePick('system'), 'the system default, picked rather than pinned').toBe('system');
    expect(devicePick(null), 'the pin standing').toBeNull();
  });

  it('reads a shape nothing here knows as the pin standing, not as a pick', () => {
    // The least-alarming reading, which is what the wire boundary takes
    // everywhere else: the row then draws the config's device and says so.
    expect(devicePick({ device_id: 'mic-2' })).toBeNull();
    expect(devicePick('microphone')).toBeNull();
  });
});
