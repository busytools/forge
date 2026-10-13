// @vitest-environment jsdom
/**
 * The mic control both surfaces wear: the button's press, the hold that opens
 * the settings, and the Escape that belongs to whatever it opened.
 *
 * Driven through a real `Dictation` over a fake socket and a microphone that
 * opens without an audio stack, because the control's job is to be the door to
 * that take - a fake driver here would test nothing the two surfaces share.
 */

import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

const mic = vi.hoisted(() => ({
  open: (): Promise<unknown> =>
    Promise.resolve({
      onFrame: null as ((bytes: Uint8Array) => void) | null,
      resolved: undefined,
      flush: () => null,
      stop: () => undefined,
    }),
}));

vi.mock('./mic', () => ({
  Microphone: { open: mic.open },
  inputs: vi.fn((): Promise<unknown[]> => Promise.resolve([])),
  inputLine: (): string => 'the inputs could not be listed \u{b7} try again',
}));

import { DEFAULT_AXES } from '../session/wire';
import type { SessionSlot } from '../wire/types';
import { Dictation } from './dictate.svelte';
import DictateControl from './DictateControl.svelte';
import { wire as fakeWire, type Wire } from './testing';

const SEAT: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };
/** How long a press must be held before it is the settings door rather than a take. */
const HOLD_MS = 450;

let app: Record<string, unknown> | null = null;

afterEach(() => {
  if (app !== null) void unmount(app);
  app = null;
  document.body.innerHTML = '';
  vi.useRealTimers();
});

interface Drawn {
  shared: Wire;
  dictate: Dictation;
  pressed: () => number;
  button: () => HTMLButtonElement;
}

/** The control over a live driver, as a surface draws it. */
function drawn(shown = false): Drawn {
  const shared: Wire = fakeWire();
  let pressed = 0;
  const dictate = new Dictation({
    connection: () => shared.connection,
    seat: () => SEAT,
    shown: () => shown,
    options: () => DEFAULT_AXES,
    device: () => null,
    onStart: () => undefined,
    onLine: () => undefined,
    onRefused: () => undefined,
    onEnded: () => undefined,
  });
  app = mount(DictateControl, {
    target: document.body,
    props: {
      dictate,
      onpress: () => {
        pressed += 1;
      },
    },
  });
  flushSync();
  return {
    shared,
    dictate,
    pressed: () => pressed,
    button: () => {
      const found = document.querySelector('.mic');
      if (!(found instanceof HTMLButtonElement)) throw new Error('the control drew no mic');
      return found;
    },
  };
}

/** A press and its release, which is what a click on a button delivers. */
function press(drawn: Drawn): void {
  const button = drawn.button();
  button.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true }));
  button.dispatchEvent(new MouseEvent('pointerup', { bubbles: true }));
  button.click();
  flushSync();
}

describe('the mic', () => {
  it('starts a take on a press, and submits on the next', async () => {
    const at = drawn();
    press(at);
    await vi.waitUntil(() => at.dictate.take !== null);

    expect(at.shared.sent[0]?.command).toEqual({
      dictate_stream: { key: SEAT, options: DEFAULT_AXES },
    });
    expect(at.button().getAttribute('aria-label'), 'the button says what the next press does').toBe(
      'stop dictating',
    );

    press(at);
    expect(at.shared.sent[1]?.command).toEqual({ dictate_stop: { key: SEAT, submit: true } });
  });

  it('says it can be stopped for a take this page did not start', () => {
    const at = drawn(true);
    expect(at.button().getAttribute('aria-label')).toBe('stop dictating');
  });

  it('tells the surface the press came from it, which is what keeps the caret', () => {
    const at = drawn();
    at.button().focus();
    press(at);
    expect(at.pressed(), 'the press is reported once').toBe(1);
    expect(document.activeElement, 'and the caret stays on the mic').toBe(at.button());
  });
});

describe('the hold', () => {
  it('opens the settings, and its release starts nothing', () => {
    vi.useFakeTimers();
    const at = drawn();
    const button = at.button();
    button.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true }));
    vi.advanceTimersByTime(HOLD_MS + 10);
    flushSync();

    expect(at.dictate.panel, 'the hold is the door').toBe(true);
    expect(at.shared.sent, 'and the hold starts no take').toEqual([]);

    button.dispatchEvent(new MouseEvent('pointerup', { bubbles: true }));
    button.click();
    flushSync();
    expect(at.dictate.panel, 'the click after the hold is not a press').toBe(true);
    expect(at.shared.sent).toEqual([]);
  });

  it('re-anchors the swallow at the release, so a long hold is still not a press', () => {
    vi.useFakeTimers();
    const at = drawn();
    const button = at.button();
    button.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true }));
    vi.advanceTimersByTime(2000);
    button.dispatchEvent(new MouseEvent('pointerup', { bubbles: true }));
    button.click();
    flushSync();

    expect(at.dictate.panel).toBe(true);
    expect(at.shared.sent).toEqual([]);
  });

  it('opens the settings on Shift+Enter, the hold door for a keyboard', () => {
    const at = drawn();
    at.button().dispatchEvent(
      new KeyboardEvent('keydown', { key: 'Enter', shiftKey: true, bubbles: true }),
    );
    flushSync();

    expect(at.dictate.panel).toBe(true);
    expect(at.shared.sent).toEqual([]);
  });
});
