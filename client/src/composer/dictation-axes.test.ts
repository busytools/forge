// @vitest-environment jsdom
/**
 * The dictation panel's own reads: the axes and their labels, the value in
 * force on each, the key hint, and what a chip asks the core for.
 *
 * Pure, so the panel's markup stays about drawing. The vocabulary is kept
 * from the view this client replaced; the form is the mockup's, which is
 * chips rather than that view's rows.
 *
 * **Named with the suffix because the card's own suite is `TakeCard.test.ts`**,
 * and on a case-insensitive filesystem `dictation.test.ts` is that file - a
 * second suite written under the lowercase name silently replaces it.
 */

import { beforeEach, describe, expect, it, vi } from 'vitest';

/** The shell's hold, as the pick module reaches it: what it asked to keep. */
const shell = vi.hoisted(() => {
  const holds: (string | null)[] = [];
  return {
    holds,
    holdShellMic: (device: string | null): Promise<void> => {
      holds.push(device);
      return Promise.resolve();
    },
    openShellMic: (): Promise<never> => Promise.reject(new Error('a browser does not open here')),
  };
});
vi.mock('./mic-shell', () => shell);

import { DEFAULT_AXES } from '../session/wire';
import {
  axesFor,
  defaultDeviceFor,
  deviceFor,
  DESTINATION,
  holdDeviceFor,
  keyHint,
  MODES,
  OVERRIDE,
  rememberAxes,
  rememberDefaultDevice,
  rememberDevice,
  STRUCTURE,
  VOICE,
} from './dictation';

beforeEach(() => {
  shell.holds.length = 0;
});

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

  it('names the field of each panel axis, which is the crate own vocabulary', () => {
    expect(OVERRIDE, 'the wire name is the Rust variant, not the panel label').toEqual({
      voice: 'styling',
      structure: 'structure',
      destination: 'context',
    });
  });

  it('reads the axes this client stored, and the config defaults where it stored none', () => {
    const seat = 'session:TestOrg/proj/lead';
    rememberAxes(seat, null);
    expect(axesFor(seat, DEFAULT_AXES), 'nothing stored is the greeting values').toEqual(
      DEFAULT_AXES,
    );

    rememberAxes(seat, { styling: 'casual', structure: 'lists', context: 'email' });
    expect(axesFor(seat, DEFAULT_AXES), 'and a stored set comes back whole').toEqual({
      styling: 'casual',
      structure: 'lists',
      context: 'email',
    });

    // A value this client is older than is the least-alarming reading, which
    // is what the wire boundary takes everywhere else.
    globalThis.localStorage.setItem(
      `forge.dictate.axes.${seat}`,
      JSON.stringify({ styling: 'shouty' }),
    );
    expect(axesFor(seat, DEFAULT_AXES).styling).toBe(DEFAULT_AXES.styling);

    rememberAxes(seat, null);
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

describe('the picked input', () => {
  it('is remembered and held in the same move, so a press never opens a device under itself', () => {
    const seat = 'session:TestOrg/proj/lead';
    rememberDevice(seat, 'coreaudio:one');
    expect(deviceFor(seat), 'the pick is what a press reads').toBe('coreaudio:one');
    expect(
      shell.holds,
      'and the shell holds the same input, or the press pays the device open again',
    ).toEqual(['coreaudio:one']);

    // Clearing the pick is the system default, which the shell holds just
    // the same: `null` is its own input, not an absence of one.
    rememberDevice(seat, null);
    expect(deviceFor(seat)).toBeNull();
    expect(shell.holds).toEqual(['coreaudio:one', null]);
  });

  it('is what a page that takes typing holds, pick or default', () => {
    const seat = 'session:TestOrg/proj/lead';
    holdDeviceFor(seat);
    expect(shell.holds, 'a seat with no pick records from the default').toEqual([null]);

    shell.holds.length = 0;
    rememberDevice(seat, 'coreaudio:two');
    shell.holds.length = 0;
    holdDeviceFor(seat);
    expect(
      shell.holds,
      'the pick already in force is the one the session start holds, so its first press is warm',
    ).toEqual(['coreaudio:two']);
  });
});

describe('the default the system resolved to', () => {
  it('is remembered once a take has opened it, and forgotten when unnamed', () => {
    expect(defaultDeviceFor(), 'nothing learned yet').toBeNull();

    rememberDefaultDevice({ id: 'mic-2', label: 'Shure SM7B' });
    expect(defaultDeviceFor()).toEqual({ id: 'mic-2', label: 'Shure SM7B' });

    // A report with no name is not a name: the row stays plain rather than
    // drawing brackets around nothing.
    rememberDefaultDevice({ id: 'mic-2', label: '' });
    expect(defaultDeviceFor()).toBeNull();

    rememberDefaultDevice(null);
    expect(defaultDeviceFor()).toBeNull();
  });
});
