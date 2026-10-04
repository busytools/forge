// @vitest-environment jsdom
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

import { DEFAULT_AXES } from '../session/wire';
import {
  axesFor,
  DESTINATION,
  keyHint,
  MODES,
  OVERRIDE,
  rememberAxes,
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
