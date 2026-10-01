/**
 * The push-to-talk key's own machine, mirroring the terminal's
 * (`crates/forge-tui/src/app/dictate_key.rs`): what a press asks for, what a
 * release asks for, and what a press that turned out to be a chord asks for.
 *
 * Pure, so the timing and the modes are pinned here rather than through a
 * mounted component - the wiring that reads keys off the window and dispatches
 * is the composer's, and its tests are there.
 */

import { describe, expect, it } from 'vitest';

import {
  bindFrom,
  boundCode,
  down,
  markChorded,
  modeFrom,
  up,
  TAP_MS,
  type Held,
} from './dictate-key';

/** A press that began at `at`, with nothing else about it yet. */
function press(at = 0, started = true): Held {
  return { since: at, chorded: false, started };
}

describe('the binding and the mode', () => {
  it('reads the vocabulary forge.toml accepts', () => {
    expect(bindFrom('left_cmd')).toBe('left_cmd');
    expect(bindFrom('off')).toBe('off');
    expect(modeFrom('toggle')).toBe('toggle');
    expect(modeFrom('hold')).toBe('hold');
  });

  it('takes an unborn value as the config default rather than as off', () => {
    // The wire carries a closed vocabulary, so the only other value is one
    // that is not there yet - a record from a server that predates the field.
    // Reading that as `off` would silently disarm every install that had not
    // set a key of its own.
    expect(bindFrom(undefined)).toBe('right_cmd');
    expect(bindFrom(null)).toBe('right_cmd');
    expect(modeFrom(undefined)).toBe('auto');
  });

  it('names the key the binding is delivered as, per platform', () => {
    // The browser reports the modifier's own side, which is what makes a
    // right-only binding possible at all.
    expect(boundCode('right_cmd', true)).toBe('MetaRight');
    expect(boundCode('left_cmd', true)).toBe('MetaLeft');
    // Off macOS there is no Cmd key, so the cmd equivalent is the right
    // Control key - the same substitution the terminal's own handler makes.
    expect(boundCode('right_cmd', false)).toBe('ControlRight');
    expect(boundCode('left_cmd', false)).toBe('ControlLeft');
    expect(boundCode('off', true)).toBeNull();
    expect(boundCode('off', false)).toBeNull();
  });
});

describe('a press and its release', () => {
  it('starts a take when nothing is recording', () => {
    const step = down(null, false, 0, 'auto');
    expect(step.action, 'the press is how a take begins').toBe('begin');
    expect(step.held, 'the press is tracked until its release').not.toBeNull();
  });

  it('asks for nothing on the press that arms a toggled recording', () => {
    const step = down(null, true, 0, 'auto');
    expect(step.action, 'a take is already running, so the press only arms its stop').toBeNull();
    expect(step.held?.started, 'this press did not start the running take').toBe(false);
  });

  it('transcribes a tap that was held past the window, and keeps a shorter one recording', () => {
    const short = up(press(0), TAP_MS - 1, 'auto');
    expect(short.action, 'a clean tap keeps the take running to be stopped later').toBeNull();

    const long = up(press(0), TAP_MS, 'auto');
    expect(long.action, 'a hold released transcribes what was said').toBe('finish');
    expect(long.held, 'the release ends the press').toBeNull();
  });

  it('transcribes a held key however brief the hold, in hold mode', () => {
    expect(up(press(0), 1, 'hold').action, 'a hold is a hold').toBe('finish');
  });

  it('stops on the press itself in toggle mode, and ignores the release', () => {
    const stopped = down(null, true, 0, 'toggle');
    expect(stopped.action, 'the press IS the stop when a take is live').toBe('finish');
    expect(stopped.held, 'nothing is left to release').toBeNull();

    const released = up(press(0), 5, 'toggle');
    expect(released.action, 'a release never stops in toggle mode').toBeNull();
  });

  it('discards the take a chorded press began, and leaves an already-running take alone', () => {
    const mine = up(markChorded(press(0)), 10, 'auto');
    expect(mine.action, 'the press turned out to be a chord, so its speculative take goes').toBe(
      'cancel',
    );

    const theirs = up(markChorded(press(0, false)), 10, 'auto');
    expect(theirs.action, 'a chord must not stop a take this press did not begin').toBeNull();
  });

  it('carries no instruction from a release with no press', () => {
    expect(up(null, 10, 'auto').action).toBeNull();
  });
});
