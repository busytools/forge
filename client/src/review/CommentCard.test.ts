// @vitest-environment jsdom
/**
 * The comment box the canvas draws on a line: the words, the two acts, and
 * the mic it wears - the same control the composer's box wears, over the
 * review page's own take.
 */

import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

const mic = vi.hoisted(() => ({
  open: (): Promise<unknown> =>
    Promise.resolve({
      onFrame: null as ((bytes: Uint8Array) => void) | null,
      resolved: undefined,
      flush: () => null,
      // The take asks the source for what it still holds when its release
      // drains (#2159); a stand-in holds nothing beyond `flush`.
      flushWorklet: () => undefined,
      stop: () => undefined,
    }),
}));

vi.mock('../composer/mic', () => ({
  Microphone: { open: mic.open },
  inputs: vi.fn((): Promise<unknown[]> => Promise.resolve([])),
  inputLine: (): string => 'the inputs could not be listed \u{b7} try again',
}));

import { Dictation, openCapture } from '../composer/dictate.svelte';
import { DRAIN_MS, FLUSH_SETTLE_MS } from '../composer/take';
import { wire as fakeWire, type Wire } from '../composer/testing';
import { DEFAULT_AXES } from '../session/wire';
import type { SessionSlot } from '../wire/types';
import CardHarness from './CardHarness.svelte';

const SEAT: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

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
  commented: string[];
  cancelled: () => number;
  typed: (text: string) => void;
  area: () => HTMLTextAreaElement;
  button: () => HTMLButtonElement;
}

/** The card over a live driver, as the canvas mounts one. */
function drawn(
  over: { line?: { tone: string; text: string } | null; busy?: boolean; dictation?: boolean } = {},
): Drawn {
  const shared: Wire = fakeWire();
  const commented: string[] = [];
  let cancelled = 0;
  const dictate = new Dictation({
    connection: () => shared.connection,
    seat: () => SEAT,
    shown: () => false,
    options: () => DEFAULT_AXES,
    device: () => null,
    capture: openCapture,
    onStart: () => undefined,
    onLine: () => undefined,
    onRefused: () => undefined,
    onEnded: () => undefined,
  });
  app = mount(CardHarness, {
    target: document.body,
    props: {
      dictate,
      slot: SEAT,
      busy: over.busy ?? false,
      line: over.line ?? null,
      dictation: over.dictation ?? true,
      oncomment: (text: string) => {
        commented.push(text);
      },
      oncancel: () => {
        cancelled += 1;
      },
    },
  });
  flushSync();
  return {
    shared,
    dictate,
    commented,
    cancelled: () => cancelled,
    typed: (text: string) => {
      const area = document.querySelector('.rv-txarea');
      if (!(area instanceof HTMLTextAreaElement)) throw new Error('the card drew no box');
      area.value = text;
      area.dispatchEvent(new Event('input', { bubbles: true }));
      flushSync();
    },
    area: () => {
      const area = document.querySelector('.rv-txarea');
      if (!(area instanceof HTMLTextAreaElement)) throw new Error('the card drew no box');
      return area;
    },
    button: () => {
      const found = document.querySelector('.mic');
      if (!(found instanceof HTMLButtonElement)) throw new Error('the card drew no mic');
      return found;
    },
  };
}

/** The acts' own buttons, by their words. */
function act(name: string): HTMLButtonElement {
  const found = [...document.querySelectorAll<HTMLButtonElement>('.rv-acts button')].find(
    (button) => button.textContent?.trim() === name,
  );
  if (found === undefined) throw new Error(`the card drew no ${name}`);
  return found;
}

describe('the comment box', () => {
  it('names the line it is anchored to, and files what was typed', () => {
    const at = drawn();
    expect(document.querySelector('.rv-where')?.textContent).toContain('write.ts');
    expect(document.querySelector('.rv-where')?.textContent).toContain('199');

    expect(act('comment').disabled, 'an empty box files nothing').toBe(true);
    at.typed('  this read is the whole input  ');
    expect(act('comment').disabled, 'and words wake it').toBe(false);

    act('comment').click();
    expect(at.commented).toEqual(['this read is the whole input']);
  });

  it('closes without filing when the cancel is pressed', () => {
    const at = drawn();
    act('cancel').click();
    expect(at.cancelled()).toBe(1);
    expect(at.commented).toEqual([]);
  });

  it('draws the line a take it could not start left behind', () => {
    drawn({ line: { tone: 'bad', text: 'no input device is available' } });
    expect(document.body.textContent).toContain('no input device is available');
  });
});

describe('the mic', () => {
  it("starts a take on the page's seat, from the box itself", async () => {
    const at = drawn();
    at.button().click();
    await vi.waitUntil(() => at.dictate.take !== null);

    expect(at.shared.sent[0]?.command).toEqual({
      dictate_stream: { key: SEAT, options: DEFAULT_AXES },
    });
    expect(at.button().getAttribute('aria-label')).toBe('stop dictating');
  });

  it('opens the settings on a hold, which the card anchors over its own box', () => {
    vi.useFakeTimers();
    const at = drawn();
    const button = at.button();
    button.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true }));
    vi.advanceTimersByTime(500);
    flushSync();

    const panel = document.querySelector('.pop');
    expect(panel, 'the hold is the door, and the panel is the shared one').not.toBeNull();
    expect(panel?.closest('article'), 'and it hangs off the card').not.toBeNull();
    expect(at.shared.sent, 'no take was started').toEqual([]);
  });

  it('cancels a live take on Escape, and lets nothing under it see the key', async () => {
    const at = drawn();
    at.button().click();
    await vi.waitUntil(() => at.dictate.take !== null);

    const event = new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true });
    window.dispatchEvent(event);
    flushSync();

    expect(at.shared.sent[1]?.command, 'an abandoned take sends no words').toEqual({
      dictate_stop: { key: SEAT, submit: false },
    });
    expect(event.defaultPrevented, 'the page under it never sees the key').toBe(true);
  });

  it('closes the settings panel when the box it was opened in goes', async () => {
    const at = drawn();
    at.dictate.panel = true;
    flushSync();
    expect(document.querySelector('.pop'), 'precondition: the panel is open').not.toBeNull();

    const page = app;
    app = null;
    await unmount(page as Record<string, unknown>);
    expect(at.dictate.panel, 'no card draws it, and nothing can close it').toBe(false);
  });

  it('closes the panel on Escape, putting the keyboard back in the box', () => {
    vi.useFakeTimers();
    const at = drawn();
    at.dictate.panel = true;
    flushSync();
    document.querySelector<HTMLButtonElement>('.pop .hd .x')?.focus();

    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    flushSync();

    expect(document.querySelector('.pop'), 'the panel is closed').toBeNull();
    expect(document.activeElement, 'and the caret is back in the box').toBe(at.area());
  });
});

describe('the bound key', () => {
  /**
   * The push-to-talk key as this platform delivers it: the config binds the
   * right Cmd, and the Control key is what stands in for it off macOS -
   * which is the platform jsdom reports.
   */
  function bound(kind: 'keydown' | 'keyup'): void {
    window.dispatchEvent(
      new KeyboardEvent(kind, { code: 'ControlRight', key: 'Control', bubbles: true }),
    );
    flushSync();
  }

  it('begins a take on the box, and submits when the press was a hold', async () => {
    vi.useFakeTimers();
    const at = drawn();
    bound('keydown');
    // The microphone's open is a promise, not a timer: let it land before
    // the release is read as this press's own.
    await vi.advanceTimersByTimeAsync(1);
    expect(at.shared.sent[0]?.command).toEqual({
      dictate_stream: { key: SEAT, options: DEFAULT_AXES },
    });

    vi.advanceTimersByTime(400);
    bound('keyup');
    // The release drains the capture before it sends its stop (#2159), so the
    // stop lands a drain window after the key rather than at it.
    vi.advanceTimersByTime(DRAIN_MS + FLUSH_SETTLE_MS + 10);
    expect(at.shared.sent[1]?.command, 'a long press transcribes on the release').toEqual({
      dictate_stop: { key: SEAT, submit: true },
    });
  });

  it('starts nothing on an install that cannot dictate', async () => {
    // The mic is not drawn there, and the key must not be a second door to a
    // take the install cannot run.
    const at = drawn({ dictation: false });
    bound('keydown');
    await Promise.resolve();

    expect(at.shared.sent, 'the key honours the install gate the mic does').toEqual([]);
  });

  it('brings the keyboard to the box before the take starts', async () => {
    const at = drawn();
    at.area().blur();
    bound('keydown');
    await vi.waitUntil(() => at.dictate.take !== null);

    expect(document.activeElement, 'the transcript lands where the keys are').toBe(at.area());
  });
});
