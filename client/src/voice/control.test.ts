// @vitest-environment jsdom
/**
 * The agent's voice switch: a click turns this client's voice on for the
 * seat, the seat's own count reaches it, and a body swap unmounts the control
 * without sending the listener away.
 */

import { readFileSync } from 'node:fs';

import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import { SLOT, wire as fakeWire, type Wire } from '../composer/testing';
import VoiceControl from './control.svelte';
import { forgetVoices, resetVoices } from './registry';

let app: Record<string, unknown> | null = null;

afterEach(() => {
  if (app !== null) void unmount(app);
  app = null;
  resetVoices();
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

/** One click, which is what a pointer or a keyboard delivers. */
function press(at: Drawn): void {
  at.button().click();
  flushSync();
}

function state(at: Drawn, listeners: number): void {
  const store = at.shared.connection.subscribe({ session: SLOT });
  store.push({ voice_mode_state: { key: SLOT, listeners } });
  flushSync();
}

// jsdom gives `import.meta.url` an http scheme, so the sheet is read from
// the client root the test runner starts in.
const SHEET = readFileSync('src/assets/web.css', 'utf8');

describe('the voice switch on the sheet', () => {
  it('takes a finger target and draws the room count', () => {
    // jsdom performs no layout, so the 44px floor and the room count are
    // pinned where they are declared rather than measured.
    const coarse = [...SHEET.matchAll(/@media \(pointer: coarse\) \{\n([\s\S]*?)\n\}/g)]
      .map((match) => match[1])
      .join('\n');
    expect(coarse, 'the voice switch left the coarse-pointer floor').toContain('.voice');
    expect(SHEET, 'the room count lost its rule').toContain('.voice .room');
  });
});

describe('the voice switch', () => {
  it('turns voice on with a click and says it is on', () => {
    const at = drawn();
    press(at);
    expect(at.shared.sent.at(-1)?.command).toEqual({
      voice_mode: { key: SLOT, on: true, explicit: false },
    });
    expect(at.button().getAttribute('aria-pressed')).toBe('true');
    expect(at.button().getAttribute('title')).toBe('tap to turn off voice');
    expect(at.button().getAttribute('aria-label')).toContain('voice on');
  });

  it('turns voice off with a second click', () => {
    const at = drawn();
    press(at);
    press(at);
    expect(at.shared.sent.at(-1)?.command).toEqual({
      voice_mode: { key: SLOT, on: false, explicit: true },
    });
    expect(at.button().getAttribute('aria-pressed')).toBe('false');
    expect(at.button().getAttribute('title')).toBe('tap to turn on voice');
  });

  it('keeps the switch when its control unmounts for a body swap', async () => {
    const at = drawn();
    press(at);
    if (app !== null) void unmount(app);
    app = null;
    flushSync();
    await new Promise((resolve) => setTimeout(resolve, 0));

    const off = at.shared.sent.filter(
      (sent) =>
        'voice_mode' in sent.command && (sent.command.voice_mode as { on: boolean }).on === false,
    );
    expect(off, 'unmounting the control is not leaving the seat').toEqual([]);
  });

  it('draws the seat count the core reports', () => {
    const at = drawn();
    press(at);
    state(at, 2);
    expect(at.button().getAttribute('aria-pressed')).toBe('true');
    expect(at.button().querySelector('.room')?.textContent, 'the room is drawn').toBe('2');
    expect(at.button().getAttribute('title')).toContain('2 clients listening');
    state(at, 1);
    expect(at.button().querySelector('.room'), 'one listener is no room to draw').toBeNull();
    state(at, 0);
    expect(at.button().getAttribute('aria-pressed')).toBe('false');
    expect(at.button().getAttribute('title')).toBe('tap to turn on voice');
  });

  it('forgets the voices bound to a connection that was replaced', () => {
    const first = drawn();
    press(first);
    if (app !== null) void unmount(app);
    app = null;
    forgetVoices();
    // **A connect-elsewhere replaces the connection under the same seat.** A
    // cached voice would speak through the socket that just closed.
    const second = drawn();
    press(second);
    expect(second.shared.sent.at(-1)?.command).toEqual({
      voice_mode: { key: SLOT, on: true, explicit: false },
    });
    expect(first.shared.sent, 'the old wire heard nothing more').toHaveLength(1);
  });
});
