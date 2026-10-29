// @vitest-environment jsdom
/**
 * The push-to-talk key over a live takeover: the page under the bar owns the
 * keyboard, so a press of the binding must not start a take behind it.
 */
import { beforeEach, describe, expect, it, vi } from 'vitest';

import { surface } from '../browser/surface.svelte';
import { type Dictation, pushToTalkHandlers, type PushToTalk } from './dictate.svelte';

/** The bound key on this platform: jsdom reports no mac. */
const BOUND = { code: 'ControlRight', key: 'Control' };

function wiring(
  over: Partial<PushToTalk> = {},
  recording = false,
): {
  wiring: PushToTalk;
  act: ReturnType<typeof vi.fn>;
  began: ReturnType<typeof vi.fn>;
} {
  const act = vi.fn();
  const began = vi.fn();
  const dictate = { held: recording, act } as unknown as Dictation;
  return {
    act,
    began,
    wiring: {
      dictate: () => dictate,
      offered: () => true,
      bind: () => 'right_cmd',
      mode: () => 'auto',
      machine: () => false,
      onBegin: began,
      ...over,
    },
  };
}

function press(handlers: ReturnType<typeof pushToTalkHandlers>): void {
  handlers.down(new KeyboardEvent('keydown', { code: BOUND.code, key: BOUND.key }));
}

function release(handlers: ReturnType<typeof pushToTalkHandlers>): void {
  handlers.up(new KeyboardEvent('keyup', { code: BOUND.code, key: BOUND.key }));
}

beforeEach(() => {
  surface.phase = 'closed';
});

describe('the push-to-talk key', () => {
  /// **A live takeover owns the keyboard.** A press while the browser surface
  /// is up must not begin a take behind the page the person is driving.
  it('starts no take while a browser takeover is up', () => {
    const { wiring: w, act, began } = wiring();
    const handlers = pushToTalkHandlers(w);
    surface.phase = 'live';
    press(handlers);
    expect(act, 'a press under a live takeover acts on nothing').not.toHaveBeenCalled();
    expect(began, 'and the keyboard is not moved to the composer').not.toHaveBeenCalled();
  });

  /// **A box standing down for a machine turn takes no keys.** The field is
  /// drawn as a line there; a press of the binding would open the microphone,
  /// and the words would land in a draft nobody can see.
  it('starts no take while a machine turn owns the box', () => {
    const { wiring: w, act, began } = wiring({ machine: () => true });
    const handlers = pushToTalkHandlers(w);
    press(handlers);
    expect(act, 'a press on a stood-down box acts on nothing').not.toHaveBeenCalled();
    expect(
      began,
      'and it does not move the keyboard into a box that is not drawn',
    ).not.toHaveBeenCalled();
  });

  /// **A machine turn refuses the begin, not the key.** A take can be in
  /// flight when the box stands down - the judgment turn's idle gate does not
  /// consider dictation, and a compaction starts whenever - so the press must
  /// still finish it: the box draws no row to end it from, and Escape
  /// discards the words.
  it('still ends a take in flight under a machine turn, in toggle mode', () => {
    const { wiring: w, act, began } = wiring({ machine: () => true, mode: () => 'toggle' }, true);
    const handlers = pushToTalkHandlers(w);

    press(handlers);

    expect(act, 'the press ends the take').toHaveBeenCalledWith('finish');
    expect(began, 'and it begins nothing').not.toHaveBeenCalled();
  });

  it('still ends a take in flight under a machine turn, in auto mode', () => {
    const { wiring: w, act, began } = wiring({ machine: () => true, mode: () => 'auto' }, true);
    const handlers = pushToTalkHandlers(w);

    press(handlers);
    expect(act, 'the press begins nothing').not.toHaveBeenCalled();

    release(handlers);
    expect(act, 'and the release ends the take').toHaveBeenCalledWith('finish');
    expect(began, 'nothing was begun at any point').not.toHaveBeenCalled();
  });

  /// **The stand-down is the takeover's, not the key's**: with no surface up
  /// the same press begins the take.
  it('starts the take again once the takeover is down', () => {
    const { wiring: w, act } = wiring();
    const handlers = pushToTalkHandlers(w);
    press(handlers);
    expect(act, 'a press with no takeover begins the take').toHaveBeenCalledWith('begin');
  });

  /// **A release still lands while a takeover is up**: a press that began
  /// before it opened has a capture waiting for this release.
  it('releases a take that a takeover opened over', () => {
    const { wiring: w, act } = wiring({ mode: () => 'hold' });
    const handlers = pushToTalkHandlers(w);
    press(handlers);
    expect(act).toHaveBeenCalledWith('begin');
    act.mockClear();
    surface.phase = 'live';
    release(handlers);
    expect(act, 'the capture begun before the takeover still ends').toHaveBeenCalledWith('finish');
  });
});
