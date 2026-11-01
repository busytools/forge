// @vitest-environment jsdom
/**
 * The voice switch: a press turns this client's voice on for the seat, the
 * seat's own count reaches it, and leaving the seat turns it back off rather
 * than leaving a listener behind.
 */

import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import { SLOT, wire as fakeWire, type Wire } from '../composer/testing';
import VoiceControl from './control.svelte';

let app: Record<string, unknown> | null = null;

afterEach(() => {
  if (app !== null) void unmount(app);
  app = null;
  document.body.innerHTML = '';
});

interface Drawn {
  shared: Wire;
  button: () => HTMLButtonElement;
}

function drawn(): Drawn {
  const shared = fakeWire();
  // The page's own subscription is what puts a store under the seat; the
  // control listens to that store rather than opening one of its own.
  shared.connection.subscribe({ session: SLOT });
  app = mount(VoiceControl, {
    target: document.body,
    props: { connection: shared.connection, slot: SLOT },
  });
  flushSync();
  return {
    shared,
    button: () => {
      const found = document.querySelector('.voice');
      if (!(found instanceof HTMLButtonElement))
        throw new Error('the control drew no voice button');
      return found;
    },
  };
}

/** A press and its release, which is what a pointer delivers. */
function press(at: Drawn): void {
  const button = at.button();
  button.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true }));
  button.dispatchEvent(new MouseEvent('pointerup', { bubbles: true }));
  flushSync();
}

function state(at: Drawn, listeners: number): void {
  const store = at.shared.connection.subscribe({ session: SLOT });
  store.push({ voice_mode_state: { key: SLOT, listeners } });
  flushSync();
}

describe('the voice switch', () => {
  it('turns voice on with a press and says it is listening', () => {
    const at = drawn();
    press(at);
    expect(at.shared.sent.at(-1)?.command).toEqual({ voice_mode: { key: SLOT, on: true } });
    expect(at.button().getAttribute('aria-pressed')).toBe('true');
    expect(at.button().getAttribute('aria-label')).toContain('listening');
  });

  it('turns voice off when the seat changes', () => {
    const at = drawn();
    press(at);
    if (app !== null) void unmount(app);
    app = null;
    flushSync();
    expect(at.shared.sent.at(-1)?.command).toEqual({ voice_mode: { key: SLOT, on: false } });
  });

  it('draws the seat count the core reports', () => {
    const at = drawn();
    press(at);
    state(at, 2);
    expect(at.button().getAttribute('aria-pressed')).toBe('true');
    state(at, 0);
    expect(at.button().getAttribute('aria-pressed')).toBe('false');
    expect(at.button().getAttribute('aria-label')).toBe('turn voice on');
  });
});
