// @vitest-environment jsdom
/**
 * The box every surface of the review page writes in: the words, the primary
 * act, the mic it wears - and the take's own bar, which takes the field's line
 * while the take runs.
 *
 * Driven through a real `Dictation` over a fake socket and a microphone that
 * opens without an audio stack, because what the box owes the reader is the
 * take's whole bargain: the composer's own mic control, the composer's own bar
 * in the field's place, and the words at the end of what is written.
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

import { openCapture, Dictation } from '../composer/dictate.svelte';
import { wire as fakeWire, type Wire } from '../composer/testing';
import { DEFAULT_AXES } from '../session/wire';
import type { SessionSlot } from '../wire/types';
import WordsHarness from './WordsHarness.svelte';

const SEAT: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };
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
  sent: string[];
  cancelled: () => number;
  typed: (text: string) => void;
  area: () => HTMLTextAreaElement | null;
  button: () => HTMLButtonElement;
  bar: () => HTMLElement | null;
}

/** The box over a live driver, as the page hands it one. */
function drawn(
  over: {
    line?: { tone: string; text: string } | null;
    dictation?: boolean;
  } = {},
): Drawn {
  const shared: Wire = fakeWire();
  const sent: string[] = [];
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
  app = mount(WordsHarness, {
    target: document.body,
    props: {
      dictate,
      slot: SEAT,
      dictation: over.dictation ?? true,
      over: { line: over.line ?? null },
      onsend: (text: string) => {
        sent.push(text);
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
    sent,
    cancelled: () => cancelled,
    typed: (text: string) => {
      const area = document.querySelector('.rv-txarea');
      if (!(area instanceof HTMLTextAreaElement)) throw new Error('the box drew no field');
      area.value = text;
      area.dispatchEvent(new Event('input', { bubbles: true }));
      flushSync();
    },
    area: () => {
      const found = document.querySelector('.rv-txarea');
      return found instanceof HTMLTextAreaElement ? found : null;
    },
    button: () => {
      const found = document.querySelector('.mic');
      if (!(found instanceof HTMLButtonElement)) throw new Error('the box drew no mic');
      return found;
    },
    bar: () => document.querySelector('.tc'),
  };
}

/** The acts' own buttons, by their words. */
function act(name: string): HTMLButtonElement {
  const found = [...document.querySelectorAll<HTMLButtonElement>('.rv-acts button')].find(
    (button) => button.textContent?.trim() === name,
  );
  if (found === undefined) throw new Error(`the box drew no ${name}`);
  return found;
}

describe('the words box', () => {
  it('names the line it is anchored to, and files what was typed', () => {
    const at = drawn();
    expect(document.querySelector('.rv-where')?.textContent).toContain('write.ts');
    expect(document.querySelector('.rv-where')?.textContent).toContain('199');

    expect(act('comment').disabled, 'an empty box files nothing').toBe(true);
    at.typed('  this read is the whole input  ');
    expect(act('comment').disabled, 'and words wake it').toBe(false);

    act('comment').click();
    expect(at.sent).toEqual(['this read is the whole input']);
  });

  it('closes without filing when the cancel is pressed', () => {
    const at = drawn();
    act('cancel').click();
    expect(at.cancelled()).toBe(1);
    expect(at.sent).toEqual([]);
  });

  it('draws the line a take it could not start left behind', () => {
    drawn({ line: { tone: 'bad', text: 'no input device is available' } });
    expect(document.body.textContent).toContain('no input device is available');
  });
});

/**
 * **The bar takes the field's own line** - the composer's rule, in this box:
 * the row the words would be typed into is the row they arrive on, so nothing
 * is added and nothing moves when a take starts or ends.
 */
describe("the take's bar", () => {
  it('takes the field line when the box begins a take, and gives it back', async () => {
    const at = drawn();
    expect(at.bar(), 'no take, no bar').toBeNull();
    expect(at.area(), 'and the field is what the box draws').not.toBeNull();

    at.button().click();
    await vi.waitUntil(() => at.dictate.take !== null);
    flushSync();

    expect(at.bar(), "the take's own bar, in the field's place").not.toBeNull();
    expect(at.area(), 'and the field it replaced is gone').toBeNull();
    expect(
      document.querySelector('.takeline'),
      'on the row the words are typed into, so nothing moves',
    ).not.toBeNull();

    // The release ends the take, and the field comes back where it was.
    at.button().click();
    await vi.waitUntil(() => at.dictate.take === null);
    flushSync();
    expect(at.bar(), 'the bar goes with the take').toBeNull();
    expect(at.area(), 'and the field is back').not.toBeNull();
  });

  it('abandons the take from the bar, sending no words', async () => {
    const at = drawn();
    at.button().click();
    await vi.waitUntil(() => at.dictate.take !== null);

    const x = document.querySelector('.tc .x');
    if (!(x instanceof HTMLButtonElement)) throw new Error('the bar drew no way out');
    x.click();
    await vi.waitUntil(() => at.shared.sent.length > 1);

    expect(at.shared.sent[1]?.command).toEqual({ dictate_stop: { key: SEAT, submit: false } });
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

  it('opens the settings on a hold, which the box anchors over itself', () => {
    vi.useFakeTimers();
    const at = drawn();
    const button = at.button();
    button.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true }));
    vi.advanceTimersByTime(HOLD_MS + 10);
    flushSync();

    const panel = document.querySelector('.pop');
    expect(panel, 'the hold is the door, and the panel is the shared one').not.toBeNull();
    expect(panel?.closest('article'), 'and it hangs off the box that drew it').not.toBeNull();
    expect(at.shared.sent, 'no take was started').toEqual([]);
  });

  it('draws no mic on an install that cannot dictate', () => {
    drawn({ dictation: false });
    expect(document.querySelector('.mic')).toBeNull();
  });
});
