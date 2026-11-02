// @vitest-environment jsdom
/**
 * The voice switch: a press turns this client's voice on for the seat, the
 * seat's own count reaches it, and a body swap unmounts the control without
 * sending the listener away.
 */

import { readFileSync } from 'node:fs';

import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import { SLOT, wire as fakeWire, type Wire } from '../composer/testing';
import { HeldClip, type VoiceCapture } from './capture.svelte';
import VoiceControl from './control.svelte';
import { resetVoices } from './registry';
import type { OpenCapture } from './voice.svelte';

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

function drawn(open?: OpenCapture): Drawn {
  const shared = fakeWire();
  // The page's own subscription is what puts a store under the seat; the
  // control listens to that store rather than opening one of its own.
  shared.connection.subscribe({ session: SLOT });
  app = mount(VoiceControl, {
    target: document.body,
    props: { connection: shared.connection, slot: SLOT, ...(open === undefined ? {} : { open }) },
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

/** A capture a test resolves by hand. */
class FakeCapture {
  clip = new HeldClip();
  cancelled = false;

  release(): Promise<HeldClip> {
    return Promise.resolve(this.clip);
  }

  cancel(): void {
    this.cancelled = true;
  }
}

// jsdom gives `import.meta.url` an http scheme, so the sheet is read from
// the client root the test runner starts in.
const SHEET = readFileSync('src/assets/web.css', 'utf8');

describe('the voice switch on the sheet', () => {
  it('takes a finger target and draws its hold meter', () => {
    // jsdom performs no layout, so the 44px floor and the meter are pinned
    // where they are declared rather than measured.
    const coarse = [...SHEET.matchAll(/@media \(pointer: coarse\) \{\n([\s\S]*?)\n\}/g)]
      .map((match) => match[1])
      .join('\n');
    expect(coarse, 'the voice switch left the coarse-pointer floor').toContain('.voice');
    expect(SHEET, 'the hold meter lost its rule').toContain('.voice .meter');
  });
});

describe('the voice switch', () => {
  it('turns voice on with a press and says it is listening', () => {
    const at = drawn();
    press(at);
    expect(at.shared.sent.at(-1)?.command).toEqual({ voice_mode: { key: SLOT, on: true } });
    expect(at.button().getAttribute('aria-pressed')).toBe('true');
    expect(at.button().getAttribute('aria-label')).toContain('listening');
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
    state(at, 1);
    expect(at.button().querySelector('.room'), 'one listener is no room to draw').toBeNull();
    state(at, 0);
    expect(at.button().getAttribute('aria-pressed')).toBe('false');
    expect(at.button().getAttribute('aria-label')).toBe('turn voice on');
  });

  it('draws the hold as talking once the microphone opens', async () => {
    const pending: { resolve: ((capture: VoiceCapture) => void) | null } = { resolve: null };
    const at = drawn(
      () =>
        new Promise((done) => {
          pending.resolve = done;
        }),
    );
    press(at);
    const button = at.button();
    button.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true }));
    await Promise.resolve();
    flushSync();
    expect(button.getAttribute('data-state'), 'the opening hold still reads listening').toBe(
      'listening',
    );

    const capture = new FakeCapture();
    pending.resolve?.(capture as unknown as VoiceCapture);
    await Promise.resolve();
    await Promise.resolve();
    flushSync();
    expect(button.getAttribute('data-state'), 'the open hold reads talking').toBe('talking');

    button.dispatchEvent(new MouseEvent('pointerup', { bubbles: true }));
    flushSync();
    expect(button.getAttribute('data-state'), 'a tap with no frames turns voice off').toBe('off');
  });
});
