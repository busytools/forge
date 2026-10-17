// @vitest-environment jsdom
/**
 * One page's own dictation: the take it starts, the presses that end it, and
 * the two cases where the press is not its own to act on.
 *
 * The driver is what the composer and the review's comment box both wear, so
 * what is pinned here is the behaviour both surfaces share rather than either
 * surface's drawing: which seat a take is addressed to, what a release during
 * the microphone's open round trip does, and which line a press leaves behind
 * when it cannot become a take.
 */

import { beforeEach, describe, expect, it, vi } from 'vitest';

/**
 * A microphone that opens without an audio stack, on Composer.test.ts's own
 * terms: what the microphone does with the frames is `take.test.ts`'s.
 */
const mic = vi.hoisted(() => {
  const held: {
    onFrame: ((bytes: Uint8Array) => void) | null;
    stops: number;
    gated: boolean;
    release: (() => void) | null;
    resolved: { id: string; label: string } | null;
    refuses: boolean;
  } = {
    onFrame: null,
    stops: 0,
    gated: false,
    release: null,
    resolved: null,
    refuses: false,
  };
  const source = {
    get onFrame(): ((bytes: Uint8Array) => void) | null {
      return held.onFrame;
    },
    set onFrame(fn: ((bytes: Uint8Array) => void) | null) {
      held.onFrame = fn;
    },
    get resolved(): { id: string; label: string } | undefined {
      return held.resolved ?? undefined;
    },
    flush: (): Uint8Array | null => null,
    // The take asks the source for what it still holds when its release
    // drains (#2159); a stand-in holds nothing beyond `flush`.
    flushWorklet: (): void => undefined,
    stop: (): void => {
      held.stops += 1;
    },
  };
  return {
    held,
    /** Open at once, wait for `held.release()` while gated, or refuse. */
    open: (): Promise<unknown> => {
      if (held.refuses) {
        return Promise.reject(Object.assign(new Error('refused'), { name: 'NotAllowedError' }));
      }
      return held.gated
        ? new Promise((resolve) => {
            held.release = () => resolve(source);
          })
        : Promise.resolve(source);
    },
  };
});

vi.mock('./mic', () => ({
  Microphone: { open: mic.open },
  inputs: vi.fn((): Promise<unknown[]> => Promise.resolve([])),
  inputLine: (): string => 'the inputs could not be listed \u{b7} try again',
  micSilence: (): Promise<null> => Promise.resolve(null),
  silenceCause: (): null => null,
}));

import { DEFAULT_AXES } from '../session/wire';
import type { SessionSlot } from '../wire/types';
import { Dictation, openCapture } from './dictate.svelte';
import { defaultDeviceFor, rememberDefaultDevice } from './dictation';
import { busyLine, microphoneLine } from './take';
import { wire as fakeWire, type Wire } from './testing';

const SEAT: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };
const ELSEWHERE: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'worker' };

/** What a case wants different about the page the driver is wired to. */
interface Held {
  seat: () => SessionSlot;
  shown: () => boolean;
  options: () => typeof DEFAULT_AXES;
  device: () => string | null;
}

/** One page's dictation, over a fake socket and the fake microphone above. */
function dictation(over: Partial<Held> = {}) {
  return wired({
    seat: () => SEAT,
    shown: () => false,
    options: () => DEFAULT_AXES,
    device: () => null,
    ...over,
  });
}

function wired(held: Held) {
  const lines: { tone: string; text: string }[] = [];
  const refusals: { tone: string; text: string }[] = [];
  const ended: { seat: string; frames: number; bytes: number; elapsedMs: number }[] = [];
  let started = 0;
  const shared: Wire = fakeWire();
  const dictate = new Dictation({
    connection: () => shared.connection,
    seat: held.seat,
    shown: held.shown,
    options: held.options,
    device: held.device,
    capture: openCapture,
    onStart: () => {
      started += 1;
      lines.length = 0;
    },
    onLine: (line) => lines.push(line),
    onRefused: (line) => refusals.push(line),
    onEnded: (counts) => ended.push(counts),
  });
  return {
    dictate,
    shared,
    lines,
    refusals,
    ended,
    started: () => started,
  };
}

beforeEach(() => {
  mic.held.onFrame = null;
  mic.held.stops = 0;
  mic.held.gated = false;
  mic.held.release = null;
  mic.held.resolved = null;
  mic.held.refuses = false;
  rememberDefaultDevice(null);
});

describe('starting a take', () => {
  it('addresses the take to the seat the page is on, with the axes in force', async () => {
    const { dictate, shared } = dictation();
    await dictate.begin();

    expect(shared.sent[0]?.command).toEqual({
      dictate_stream: { key: SEAT, options: DEFAULT_AXES },
    });
    expect(dictate.take, 'and the page holds the take it started').not.toBeNull();
  });

  it('refuses a second take by the name of the one holding the microphone', async () => {
    const { dictate, shared, refusals } = dictation();
    await dictate.begin();
    await dictate.begin();

    expect(shared.sent, 'the second press starts nothing').toHaveLength(1);
    expect(refusals, 'a refusal belongs where the reader is now').toEqual([
      { tone: 'bad', text: busyLine(SEAT) },
    ]);
  });

  it('says why when the microphone will not open, and holds no take', async () => {
    mic.held.refuses = true;
    const { dictate, lines, refusals } = dictation();
    await dictate.begin();

    expect(dictate.take).toBeNull();
    expect(lines, "a take's own failure belongs where the attempt was made").toEqual([
      { tone: 'bad', text: microphoneLine({ name: 'NotAllowedError' }) },
    ]);
    expect(refusals).toEqual([]);
  });

  it('learns the input a plain take resolved to, which is the only place it is named', async () => {
    mic.held.resolved = { id: 'mic-2', label: 'Shure SM7B' };
    const { dictate } = dictation();
    await dictate.begin();

    expect(defaultDeviceFor()).toEqual({ id: 'mic-2', label: 'Shure SM7B' });
  });

  it('leaves what the default was alone when the take opened a picked input', async () => {
    mic.held.resolved = { id: 'mic-9', label: 'Built-in' };
    rememberDefaultDevice({ id: 'mic-2', label: 'Shure SM7B' });
    const { dictate } = dictation({ device: () => 'mic-9' });
    await dictate.begin();

    expect(defaultDeviceFor(), 'a pick is not the default').toEqual({
      id: 'mic-2',
      label: 'Shure SM7B',
    });
  });
});

describe('the press', () => {
  it('begins on a press with nothing live, and submits on the next', async () => {
    const { dictate, shared } = dictation();
    dictate.toggle();
    await vi.waitUntil(() => dictate.take !== null);

    dictate.toggle();
    // The release drains the capture before it sends its stop (#2159), so the
    // stop lands a drain window after the press rather than at it.
    await vi.waitUntil(() => shared.sent.length > 1);
    expect(shared.sent[1]?.command).toEqual({ dictate_stop: { key: SEAT, submit: true } });
    expect(dictate.take, 'and the page is holding nothing').toBeNull();
  });

  it('ends a take this page did not start, which the record still shows', () => {
    const { dictate, shared } = dictation({ shown: () => true });
    dictate.toggle();

    expect(shared.sent[0]?.command).toEqual({ dictate_stop: { key: SEAT, submit: true } });
  });

  it('refuses to end a take that belongs to another seat', async () => {
    let seat = SEAT;
    const { dictate, shared, refusals } = dictation({ seat: () => seat });
    await dictate.begin();
    seat = ELSEWHERE;

    dictate.toggle();
    expect(shared.sent, 'nothing is stopped for the seat on screen').toHaveLength(1);
    expect(refusals).toEqual([{ tone: 'bad', text: busyLine(SEAT) }]);
  });

  it('holds a release that lands while the microphone is still opening', async () => {
    mic.held.gated = true;
    const { dictate, shared } = dictation();

    dictate.toggle();
    dictate.toggle();
    expect(shared.sent, 'nothing has opened yet').toHaveLength(0);

    mic.held.release?.();
    await vi.waitUntil(() => shared.sent.length > 1);

    expect(shared.sent[0]?.command).toEqual({
      dictate_stream: { key: SEAT, options: DEFAULT_AXES },
    });
    expect(shared.sent[1]?.command, 'the release is what the gesture asked for').toEqual({
      dictate_stop: { key: SEAT, submit: true },
    });
    expect(dictate.take, 'and the take is over').toBeNull();
  });
});

describe('the bound key', () => {
  it('begins a take, which is what the key asks for', async () => {
    const { dictate, shared } = dictation();
    dictate.act('begin');
    await vi.waitUntil(() => dictate.take !== null);

    expect(shared.sent[0]?.command).toEqual({
      dictate_stream: { key: SEAT, options: DEFAULT_AXES },
    });
  });

  it('cancels a take that is live', async () => {
    const { dictate, shared } = dictation();
    await dictate.begin();
    dictate.act('cancel');

    expect(shared.sent[1]?.command).toEqual({ dictate_stop: { key: SEAT, submit: false } });
  });
});
