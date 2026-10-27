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

function wiring(over: Partial<PushToTalk> = {}): {
  wiring: PushToTalk;
  act: ReturnType<typeof vi.fn>;
  began: ReturnType<typeof vi.fn>;
} {
  const act = vi.fn();
  const began = vi.fn();
  const dictate = { held: false, act } as unknown as Dictation;
  return {
    act,
    began,
    wiring: {
      dictate: () => dictate,
      offered: () => true,
      bind: () => 'right_cmd',
      mode: () => 'auto',
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
