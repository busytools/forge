// @vitest-environment jsdom
import { readFileSync } from 'node:fs';
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { hideBrowser, onModifier, onModifierChord, showBrowser } from '../browser/link';
import { openSurface, selectTab, surface } from '../browser/surface.svelte';

/**
 * The shell's own door, mock-able so both halves of Open's claim are testable
 * here: outside the shell the real one answers a reason, which only ever
 * proved the could-not line.
 */
vi.mock('../browser/link', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../browser/link')>();
  return {
    ...actual,
    // jsdom is not the shell: the surface gates would answer "not the
    // client" and the takeover's tests would never see their bar. The store
    // and the shell's own doors are mocked below on that basis.
    canAttach: vi.fn(() => true),
    drawsSurface: vi.fn(() => true),
    // No window in tests: the shell's raise answers the reason it could not
    // land, or null when it did. The default takes it as landing; a case
    // that wants the failure resolves the reason instead.
    showBrowser: vi.fn(() => Promise.resolve({ profile: 'job-hunt', tab: 2 })),
    hideBrowser: vi.fn(() => Promise.resolve(true)),
    // The overlay's own key route, opened by the composer while a takeover is
    // up. jsdom has no responder chain to deliver it from, so a case fires
    // the handler this recorded.
    onModifier: vi.fn((_fn: (press: { code: number; down: boolean }) => void) =>
      Promise.resolve(() => {}),
    ),
    onModifierChord: vi.fn((_fn: () => void) => Promise.resolve(() => {})),
  };
});

/**
 * A microphone that opens without an audio stack.
 *
 * jsdom has no `getUserMedia`, and these cases are about the key, the
 * dispatch and the record rather than the audio: what the microphone DOES
 * with the frames is `capture.test.ts`'s and `take.test.ts`'s, and what it
 * costs to open one is nobody's here.
 */
const mic = vi.hoisted(() => {
  const held: {
    onFrame: ((bytes: Uint8Array) => void) | null;
    stops: number;
    asked: number;
    gated: boolean;
    refuse: boolean;
    release: (() => void) | null;
    resolved: { id: string; label: string } | null;
  } = {
    onFrame: null,
    stops: 0,
    asked: 0,
    gated: false,
    refuse: false,
    release: null,
    resolved: null,
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
    flushWorklet: (): void => {
      held.asked += 1;
    },
    stop: (): void => {
      held.stops += 1;
    },
  };
  return {
    held,
    /**
     * Open at once, or wait for `held.release()` while gated - or reject
     * outright while `held.refuse`, which is the permission the reader
     * refused.
     */
    open: (): Promise<unknown> => {
      if (held.refuse) return Promise.reject(new DOMException('refused', 'NotAllowedError'));
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
  // A walk a case drives: `mockResolvedValueOnce`/`mockRejectedValueOnce`.
  inputs: vi.fn((): Promise<unknown[]> => Promise.resolve([])),
  inputLine: (): string => 'the inputs could not be listed \u{b7} try again',
  // The cause readers live in the shell, and jsdom has no shell: a silent
  // capture's row draws the generic line here, which is the browser's own case.
  micSilence: (): Promise<null> => Promise.resolve(null),
  silenceCause: (): null => null,
}));

import Harness from './Harness.svelte';
import { encodeFrame } from './capture.svelte';
import { echoes, type Echo } from '../chat/echoes.svelte';
import { pinnedRow } from '../chat/pinnedRow.svelte';
import type { TurnInfo } from '../chat/units';
import { boxKey } from './box.svelte';
import { modelsWire } from '../models/testing';
import { subjectKey, type ServerMessage, type Subject } from '../protocol';
import { DEFAULT_AXES, type DictateAxes } from '../session/wire';
import {
  browserHandOffAsk,
  documentHandOffAsk,
  permissionAsk,
  questionAsk,
  record,
  seatRead,
  SLOT,
  slackDraftAsk,
  take,
  wire,
  type Wire,
} from './testing';
import { DEFAULT_SETTINGS, type SessionSlot } from '../wire/types';
import { axesFor, deviceFor, rememberAxes, rememberDevice } from './dictation';
import { inputs } from './mic';
import { DRAIN_MS, FLUSH_SETTLE_MS } from './take';
import { SILENT_NOTICE_MS } from './dictate.svelte';
import { TRUNCATED, type ComposerProps, type ComposerRecord, type SeatRead } from './view';

/**
 * A submitted take's release, past its drain.
 *
 * The release holds the capture open for the drain window and asks the
 * worklet for what it still holds, and only then sends the stop - so a case
 * that reads what went on the wire after a release advances the clock first.
 */
function settle(): void {
  vi.advanceTimersByTime(DRAIN_MS + FLUSH_SETTLE_MS + 10);
}

/** Everything the page is drawing, as a reader reads it. */
const drawn = () => document.body.textContent ?? '';

/** The box's field, which every state but a dock and a blocked box draws. */
function field(): HTMLTextAreaElement {
  const found = document.querySelector('textarea');
  if (!(found instanceof HTMLTextAreaElement)) {
    throw new Error('the composer is not drawing its field');
  }
  return found;
}

/** Type into the box, as a reader does - the value is the browser's, the state is ours. */
function type(text: string): void {
  const box = field();
  box.value = text;
  box.dispatchEvent(new Event('input', { bubbles: true }));
  flushSync();
}

/**
 * Press a key on whatever holds the keyboard, which the box and the dock take
 * turns at - a real key goes to the focused element, and a dock that nothing
 * focuses is a dock whose keys do nothing.
 */
function press(key: string): void {
  const target = document.activeElement;
  if (!(target instanceof HTMLElement)) throw new Error('nothing holds the keyboard');
  target.dispatchEvent(new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true }));
  flushSync();
}

/**
 * The bound key's own event, as the browser delivers it.
 *
 * jsdom reports no platform, so the box takes the non-mac substitution: the
 * right Control key is what Cmd's equivalent is where there is no Cmd.
 */
function key(code: string, kind: 'keydown' | 'keyup', repeat = false): void {
  window.dispatchEvent(new KeyboardEvent(kind, { code, repeat, bubbles: true, cancelable: true }));
  flushSync();
}

/**
 * Let a take's start settle.
 *
 * Opening the microphone is a promise - the browser's permission round trip -
 * and the command goes once it resolves, so a test that read the wire on the
 * same tick would see a take that had not started.
 */
async function opened(): Promise<void> {
  for (let turn = 0; turn < 4; turn += 1) await Promise.resolve();
  flushSync();
}

let app: Record<string, unknown> | null = null;
let second: Record<string, unknown> | null = null;

afterEach(() => {
  if (app !== null) void unmount(app);
  if (second !== null) void unmount(second);
  app = null;
  second = null;
  // **`mockClear` is not enough here**: it drops the call history but keeps
  // both an unconsumed `mockResolvedValueOnce` and a persistent implementation,
  // so either would decide the next case's open or lower. Both doors are reset
  // and re-armed with the defaults the module mock declares.
  vi.mocked(showBrowser).mockReset();
  vi.mocked(showBrowser).mockImplementation(() => Promise.resolve({ profile: 'job-hunt', tab: 2 }));
  vi.mocked(hideBrowser).mockReset();
  vi.mocked(hideBrowser).mockImplementation(() => Promise.resolve(true));
  document.body.innerHTML = '';
  // The pending send outlives the page that drew it, so a case that leaves one
  // behind would hand it to the next.
  echoes.clear(boxKey(SLOT));
  echoes.clear(boxKey(ELSEWHERE));
  // The overlay's key doors are this file's own state too: a registration
  // left in the history would answer the next case's mount.
  vi.mocked(onModifier).mockClear();
  vi.mocked(onModifierChord).mockClear();
});

/** The send a seat is waiting on, which the conversation draws and the box does not. */
function echoAt(slot: SessionSlot): Echo | undefined {
  return echoes.of(boxKey(slot));
}

/** The harness's own state, which a test sets the way a page would re-render it. */
interface Page {
  record: ComposerRecord;
  seat: SeatRead;
  /** The seat the page is showing, which moves with the record as a reader does. */
  slot: SessionSlot;
}

function pageOf(instance: Record<string, unknown>): Page {
  const held = instance['page'];
  if (held === null || typeof held !== 'object') throw new Error('the harness exposed no props');
  return held as Page;
}

function open(over: Partial<ComposerProps> = {}, on?: Wire) {
  const shared = on ?? wire();
  app = mount(Harness, {
    target: document.body,
    props: { wire: shared, initial: over, dictation: over.dictation ?? false },
  });
  flushSync();
  return {
    page: pageOf(app),
    sent: shared.sent,
    say: (message: ServerMessage) => shared.say(message),
  };
}

/** The phone and the desktop: two composers, one seat, one connection. */
function openBoth(over: Partial<ComposerProps> = {}) {
  const shared = wire();
  const one = open(over, shared);
  second = mount(Harness, {
    target: document.body,
    props: { wire: shared, initial: over, dictation: over.dictation ?? false },
  });
  flushSync();
  return {
    shared,
    one,
    other: {
      page: pageOf(second),
      sent: shared.sent,
      say: (message: ServerMessage) => shared.say(message),
    },
  };
}

/** A record whose composer holds what a case wants it to. */
function withNotice(
  notice: unknown,
  held: Record<string, unknown> | null = null,
  slot: SessionSlot = SLOT,
): ComposerRecord {
  return record({ slot, composer: { take: held, notice, machine: null } });
}

/** A take that landed, as the core sends one: the words, and whether they were cut. */
const LANDED = { kind: 'landed', text: 'push it once CI is green', truncated: false };

/** A seat that is not the one under test, which is the one a reader moves to. */
const ELSEWHERE: SessionSlot = { org: 'Busytools', project: 'forge', label: 'other' };

/** Send what is in the box, as the reader's Enter does. */
function sendBox(): void {
  field().dispatchEvent(
    new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }),
  );
  flushSync();
}

/** The frame a turn opens with, which is where the core says a turn is in flight. */
function openTurn(): Record<string, unknown> {
  return { type: 'system', subtype: 'init', session_id: 's' };
}

/**
 * Begin a take in THIS composer's own box, as its push-to-talk key does - what
 * arms its landing - show it on the record, and let the core's own word end
 * it, so the box a take was drawing is a box again.
 *
 * The press is what makes the take this page's own: the landing's notice is
 * the seat's and reaches every client that reads the record, so the arm has to
 * come from this page's own capture rather than from what the record shows.
 */
async function beginTake(
  harness: { page: { record: ComposerRecord }; say: (message: ServerMessage) => void },
  seat: SessionSlot = SLOT,
): Promise<void> {
  key('ControlRight', 'keydown');
  await opened();
  // A tap's release keeps the take recording, so the press in flight is done
  // with and the next press begins rather than being swallowed.
  key('ControlRight', 'keyup');
  showTake(harness);
  endTake(harness, seat);
}

/**
 * The same on ONE client of a two-client pair, through its own mic.
 *
 * The key is a window event: with both clients mounted one press would begin
 * a take on each, and the pair's whole subject is the take exactly one of
 * them began.
 */
async function beginTakeOn(
  at: number,
  harness: { page: { record: ComposerRecord }; say: (message: ServerMessage) => void },
  seat: SessionSlot = SLOT,
): Promise<void> {
  const button = [...document.querySelectorAll('.mic')][at];
  if (!(button instanceof HTMLElement)) throw new Error(`the client at ${String(at)} drew no mic`);
  button.click();
  await opened();
  showTake(harness);
  endTake(harness, seat);
}

/** The record a begun take leaves: the seat's composer showing it. */
function showTake(harness: { page: { record: ComposerRecord } }): void {
  const held = harness.page.record;
  const composer = typeof held.composer === 'object' && held.composer !== null ? held.composer : {};
  harness.page.record = { ...held, composer: { ...composer, take: take(), notice: null } };
  flushSync();
}

/**
 * The core's own word ends the take, as it does when one lands.
 *
 * The outcome is the core's own shape, externally tagged with what the take
 * produced. The words in it are nobody's here - each case asserts on the
 * landing it hands over itself - but the frame is the one the core sends.
 */
function endTake(
  harness: { say: (message: ServerMessage) => void },
  seat: SessionSlot = SLOT,
): void {
  harness.say({
    kind: 'update',
    update: {
      dictate_ended: {
        key: seat,
        outcome: { landed: { text: 'the words', truncated: false } },
        generation: 1,
      },
    },
  });
  flushSync();
}

/** Both clients' boxes, in mount order, which is one field per composer. */
function bothFields(): HTMLTextAreaElement[] {
  return [...document.querySelectorAll('textarea')].map((found) => {
    if (!(found instanceof HTMLTextAreaElement)) throw new Error('a box drew no field');
    return found;
  });
}

/**
 * One question of a batch, with its options named for the question they belong
 * to.
 *
 * Distinct ids per question because the wire's are positional: a stale one from
 * question one is a valid id for question two's same row, which is what makes
 * carrying it over silent rather than rejected.
 */
function oneOf(ids: string[], index: number): unknown {
  return questionAsk(
    'tu-q',
    {
      multi_select: true,
      options: ids.map((optionId, at) => ({
        option_id: optionId,
        label: `Row ${String(at + 1)}`,
        description: null,
        preview: null,
      })),
    },
    index,
    2,
  );
}

/** Every option row the page is drawing, in order. */
function options(): HTMLElement[] {
  return [...document.querySelectorAll('.opt')].map((row) => {
    if (!(row instanceof HTMLElement)) throw new Error('an option row is not an element');
    return row;
  });
}

/** A held post's verb, or a permission's action: both are buttons, not rows. */
function action(label: string): HTMLElement {
  const found = [...document.querySelectorAll('.dock .acts button')].find((button) =>
    button.textContent?.trim().startsWith(label),
  );
  if (!(found instanceof HTMLElement)) throw new Error(`the dock drew no ${label}`);
  return found;
}

/** A button on the takeover's bar, which the composer draws outside the dock. */
function barAction(label: string): HTMLElement {
  const found = [...document.querySelectorAll('.takeover-bar .tb-btn')].find((button) =>
    button.textContent?.trim().startsWith(label),
  );
  if (!(found instanceof HTMLElement)) throw new Error(`the takeover bar drew no ${label}`);
  return found;
}

/** Every label the takeover's bar is drawing, which is what a person can act on. */
function barButtons(): string[] {
  return [...document.querySelectorAll('.takeover-bar .tb-btn')].map(
    (button) => button.textContent?.trim() ?? '',
  );
}

/** What the takeover's bar itself is saying - never the chat drawn behind it. */
function barText(): string {
  return document.querySelector('.takeover-bar')?.textContent ?? '';
}

/**
 * A surface raised for one seat and tab, as Open, the strip's row or a raise
 * for another seat leaves it. The shell's answer is the suite's mock.
 */
async function surfaceOn(seat: SessionSlot, tab: number): Promise<void> {
  vi.mocked(showBrowser).mockResolvedValueOnce({ profile: 'job-hunt', tab });
  await openSurface(null, seat, tab);
  flushSync();
}

/**
 * Let an Open land. **The bar's answers ride the page, not the press**: Done,
 * Not now and the feedback line are offered once the surface is live on the
 * tab the ask named, which is the shell's answer away.
 */
async function raised(): Promise<void> {
  await vi.waitFor(() => expect(surface.phase, 'the page the ask named is up').toBe('live'));
  flushSync();
}

/**
 * Begin a take at the takeover's own mic, which is the one door a take has
 * while the page owns the keyboard, and end it with the core's word.
 */
async function beginBarTake(harness: {
  page: { record: ComposerRecord };
  say: (message: ServerMessage) => void;
}): Promise<void> {
  const mic = document.querySelector('.takeover-bar .micb');
  if (!(mic instanceof HTMLElement)) throw new Error('the bar drew no mic');
  mic.click();
  await opened();
  showTake(harness);
  endTake(harness);
}

/** Start a take at the bar's mic and leave it recording. */
async function startBarTake(): Promise<void> {
  const mic = document.querySelector('.takeover-bar .micb');
  if (!(mic instanceof HTMLElement)) throw new Error('the bar drew no mic');
  mic.click();
  await opened();
}

/** The shell's answer to a raise, as the mocked door carries it. */
type OpenAnswer = Awaited<ReturnType<typeof showBrowser>>;

/**
 * Press a failed page's retry, holding the shell's answer so the surface
 * stays `opening` until the case lands it.
 */
function retryHeld(): { land: (answer: OpenAnswer) => void } {
  let land: (answer: OpenAnswer) => void = () => {};
  vi.mocked(showBrowser).mockReturnValueOnce(
    new Promise<OpenAnswer>((resolve) => {
      land = resolve;
    }),
  );
  barAction('try again').click();
  flushSync();
  return { land };
}

/**
 * End the take and let its words land on the page as it stands now, with
 * whatever ask is still parked.
 */
function landBarTake(
  harness: { page: { record: ComposerRecord }; say: (message: ServerMessage) => void },
  asks: unknown[] = [browserHandOffAsk()],
): void {
  showTake(harness);
  endTake(harness);
  harness.page.record = record({
    pending_asks: asks,
    composer: {
      take: null,
      notice: { kind: 'landed', text: 'the words', truncated: false },
      machine: null,
    },
  });
  flushSync();
}

/**
 * The one behaviour here whose failure destroys something a person typed.
 *
 * The box morphs into the dock, and the draft is the client's own state rather
 * than the field's, so the box coming back is the same person's words. This is
 * the input-loss defect the project already has an incident for, which is why
 * it is a test rather than an intention.
 */
beforeEach(() => {
  // The surface store is the page's own state; each case starts with nothing
  // open, exactly as a fresh page does - and with no seat, tab or ask left
  // from the case before it.
  surface.phase = 'closed';
  surface.profile = null;
  surface.reason = null;
  surface.id = null;
  surface.tab = null;
  surface.seat = null;
  surface.tabs = null;
  surface.attachedProfile = null;
  surface.viewer = null;
  surface.controller = null;
  surface.nav = { can_back: false, can_forward: false };
  vi.mocked(onModifier).mockClear();
  vi.mocked(onModifierChord).mockClear();
});

describe("the reader's draft", () => {
  it('is held while a prompt has the box, and comes back unchanged when the prompt is answered', () => {
    const harness = open();
    type('fix the flaky retry test');

    harness.page.record = record({ pending_asks: [permissionAsk()] });
    flushSync();

    expect(
      document.querySelector('[data-editor="composer"]'),
      'the dock morphs the box, so the composer draws no field of its own',
    ).toBeNull();
    expect(drawn(), 'the prompt itself is what the slot draws').toContain('Allow once');

    const answered = document.querySelector('.acts .btn');
    if (!(answered instanceof HTMLElement))
      throw new Error('the dock drew no action to answer with');
    answered.click();
    flushSync();

    harness.page.record = record();
    flushSync();

    expect(field().value, 'the reader typed this and the dock took it').toBe(
      'fix the flaky retry test',
    );
  });

  it('is held across a prompt the reader rejected', () => {
    const harness = open();
    type('ship it once CI is green');

    harness.page.record = record({ pending_asks: [permissionAsk()] });
    flushSync();

    options()[1]?.click();
    flushSync();

    harness.page.record = record();
    flushSync();

    expect(field().value, 'the draft went with the prompt').toBe('ship it once CI is green');
  });
});

/**
 * The box's other states: the take that lives inside it, the notice a take
 * leaves, and the reasons the whole thing is replaced.
 */
describe('the box', () => {
  it('sends what the reader typed, and clears the box for the next thing', () => {
    const harness = open();
    type('push it once CI is green');
    const box = field();
    box.dispatchEvent(
      new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }),
    );
    flushSync();

    const [sent] = harness.sent;
    const under = sent?.command['prompt_under'] ?? {};
    expect(under['key'], 'the command is the text as typed, addressed to this seat').toEqual({
      org: 'Busytools',
      project: 'forge',
      label: 'lead',
    });
    expect(under['text']).toBe('push it once CI is green');
    expect(under['attachments']).toEqual([]);
    expect(under['source']).toBe('you');
    expect(
      typeof under['uuid'],
      "the id is the send's own, minted here so the CLI's lifecycle frames and the queued row carry it back",
    ).toBe('string');
    expect(field().value, 'the box is empty once the words have gone').toBe('');
  });

  it('hands an empty box up to the pile, and only an empty one', () => {
    // The page's own shape: the queue rides the chat column and the box sits
    // in the composer bar, both under the page root - the container the
    // up-entry reaches across. The pile is the minimal thing the query finds
    // - what the walk does once it has the keyboard is Queue's own test.
    const shell = document.createElement('div');
    shell.className = 'session-page';
    const column = document.createElement('div');
    column.className = 'chat';
    const pile = document.createElement('div');
    pile.className = 'pile';
    const list = document.createElement('div');
    list.setAttribute('role', 'listbox');
    list.tabIndex = -1;
    pile.append(list);
    column.append(pile);
    shell.append(column);
    const bar = document.createElement('div');
    bar.className = 'composer';
    const target = document.createElement('div');
    bar.append(target);
    shell.append(bar);
    document.body.append(shell);
    app = mount(Harness, { target, props: { wire: wire(), initial: {}, dictation: false } });
    flushSync();

    field().focus();
    press('ArrowUp');
    expect(document.activeElement, 'the empty box hands the keyboard to the pile').toBe(list);

    field().focus();
    type('a draft');
    press('ArrowUp');
    expect(document.activeElement, 'a draft keeps the key for its own lines').toBe(field());
  });

  it('sends on a plain-http origin, where randomUUID does not exist', () => {
    // This client is reached over a LAN, and `crypto.randomUUID` is
    // secure-context-only: the send has to leave under a real id anyway, which
    // is what the mint's own fallbacks are for. Stubbed at the global rather
    // than the helper, because the helper being right is not the send being
    // right.
    const real = globalThis.crypto;
    Object.defineProperty(globalThis, 'crypto', {
      value: { getRandomValues: real.getRandomValues.bind(real) },
      configurable: true,
    });
    try {
      const harness = open();
      type('no secure context here');
      field().dispatchEvent(
        new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }),
      );
      flushSync();

      const [sent] = harness.sent;
      const under = sent?.command['prompt_under'] ?? {};
      expect(under['text'], 'the words went').toBe('no secure context here');
      expect(typeof under['uuid'], 'and an id went with them').toBe('string');
      expect(under['uuid'], 'a real one, not a blank').not.toBe('');
    } finally {
      Object.defineProperty(globalThis, 'crypto', { value: real, configurable: true });
    }
  });

  /**
   * The one control that is not the reader's words.
   *
   * A turn in flight is the state the core reports, so the control follows
   * that rather than anything this composer sent: a turn a cron or another
   * page started is the same turn to stop. It is drawn with an empty box,
   * which is exactly when a reader wants it.
   */
  it('stops the running turn, and offers the control only while one runs', () => {
    const harness = open({ record: record({ header: { turn_in_flight: true } }) });

    const stop = document.querySelector('.stop');
    expect(stop, 'a turn in flight draws the stop, empty box and all').not.toBeNull();
    (stop as HTMLButtonElement).click();
    flushSync();

    expect(harness.sent, 'the stop is addressed to this seat, like a send').toEqual([
      {
        command: { cancel: { key: { org: 'Busytools', project: 'forge', label: 'lead' } } },
      },
    ]);

    harness.page.record = record({ header: { turn_in_flight: false } });
    flushSync();
    expect(document.querySelector('.stop'), 'no turn, no control').toBeNull();
  });

  /**
   * **Escape is the stop's other door** (Ved, 2026-10-15). The view this
   * client replaced bound the key to the same dispatch the button makes, and
   * this client never wired it - so the key fell through to the OS and left
   * fullscreen instead. A take consumes the key first: one press is one
   * command.
   */
  it('stops the running turn on Escape, with a take keeping the key before it', () => {
    const harness = open({
      dictation: true,
      record: record({ header: { turn_in_flight: true } }),
    });

    press('Escape');
    expect(harness.sent, 'Escape stops the turn the button would').toEqual([
      {
        command: { cancel: { key: { org: 'Busytools', project: 'forge', label: 'lead' } } },
      },
    ]);

    // A live take keeps the key for itself: the press drops the take, and
    // the running turn is left alone.
    harness.sent.length = 0;
    harness.page.record = record({
      header: { turn_in_flight: true },
      composer: { take: take(), notice: null, machine: null },
    });
    flushSync();
    press('Escape');
    expect(
      harness.sent.some(
        (sent) => 'command' in sent && 'cancel' in (sent.command as Record<string, unknown>),
      ),
      'the turn keeps running: the take took the key',
    ).toBe(false);
  });

  /**
   * **The key is not the dictation toggle's.** An install with dictation off
   * still has the turn's stop and its Escape must still reach it - that
   * install is exactly the one whose key fell through to the OS, so the
   * gate's `!running` half is half the fix.
   */
  it('stops the running turn on Escape with dictation off', () => {
    const harness = open({ record: record({ header: { turn_in_flight: true } }) });

    press('Escape');
    expect(harness.sent, 'the stop is not behind the dictation toggle').toEqual([
      {
        command: { cancel: { key: { org: 'Busytools', project: 'forge', label: 'lead' } } },
      },
    ]);
  });

  /**
   * **The list under the field keeps its own Escape** (the rule the take
   * case above states): it closes on a key nobody else took, and the turn -
   * a stop the reader could also click - is not a claim above a surface they
   * are reading. One press closes the list and stops nothing.
   */
  it('gives Escape to the list rather than to the running turn', () => {
    const harness = open({ record: record({ header: { turn_in_flight: true } }) });
    type('/');
    expect(document.querySelector('.ac'), 'a list is open under the field').not.toBeNull();

    press('Escape');

    expect(harness.sent, 'the turn keeps running: the list took the key').toEqual([]);
    expect(document.querySelector('.ac'), 'and the list closed').toBeNull();
  });

  /**
   * `/resume`'s list is the record's, drawn by the one popover and written by
   * the one pick: a record the composer never reads is a popover that opens
   * nothing, and nothing on screen says the field was the missing link.
   */
  it("offers the seat's resumable sessions after /resume and picks one by id", () => {
    open({
      record: record({
        resume_sessions: [
          { session_id: '7f3a92e0-1c2b', summary: 'the resume picker' },
          { session_id: 'c40d18aa-2b3c', summary: 'the auth timeout fix' },
        ],
      }),
    });

    type('/resume ');
    const list = document.querySelector('.ac');
    expect(list, 'the space opens the list').not.toBeNull();
    expect(list?.textContent, 'the ids are drawn, and what each one was').toContain(
      '7f3a92e0-1c2b',
    );
    expect(list?.textContent).toContain('the auth timeout fix');

    type('/resume 7f');
    expect(
      document.querySelector('.ac')?.textContent,
      'a partial narrows it to the one id it starts',
    ).not.toContain('c40d18aa-2b3c');

    press('Enter');

    expect(document.querySelector('.ac'), 'the pick closes the list').toBeNull();
    expect(field().value, 'and leaves the ordinary command, ready to send').toBe(
      '/resume 7f3a92e0-1c2b ',
    );
  });

  it("draws the take on the draft's own line, and takes it away when it resolves", () => {
    const harness = open();

    harness.page.record = record({
      composer: { take: take(), notice: null, machine: null },
    });
    flushSync();

    const card = document.querySelector('.line .tc');
    expect(card, "the take did not take the draft's own line").not.toBeNull();
    expect(
      document.querySelector('.line textarea'),
      'the field still draws beside a live take',
    ).toBeNull();
    expect(document.querySelector('.foot'), 'the take grew a second row').toBeNull();

    harness.page.record = record();
    flushSync();

    expect(document.querySelector('.tc'), 'the card goes with the take').toBeNull();
    expect(document.querySelector('.line textarea'), 'and the field comes back').not.toBeNull();
  });

  /**
   * **A take is the body's row, so the controls ride it.** The card takes the
   * draft's own line, the controls ride that line, and the band's height is the
   * same before, during and after a press. The hints stand down with the field
   * they name: the take unmounted it, and a hint about keys that do nothing is
   * worse than a row without them.
   */
  it('keeps the controls on the take row over a draft, drawn once', async () => {
    const harness = open({ dictation: true });
    type('fix the');

    // A live take over the draft, which is the press's own state: the capture
    // is open and the record shows the take.
    key('ControlRight', 'keydown');
    await opened();
    key('ControlRight', 'keyup');
    showTake(harness);

    expect(document.querySelector('.line .tc'), "the card takes the draft's line").not.toBeNull();
    expect(document.querySelector('.line .ctls .mic'), 'the controls ride the row').not.toBeNull();
    expect(
      document.querySelectorAll('.comp .ctls'),
      'the controls are drawn once, not beside a second row',
    ).toHaveLength(1);
    expect(document.querySelector('.foot'), 'a second row came back under the take').toBeNull();
    expect(
      document.querySelector('.line .hints'),
      'the key hints promise keys a field the take unmounted cannot take',
    ).toBeNull();
  });

  it('draws no wire counts for a take this page did not start', () => {
    const harness = open();
    harness.page.record = withNotice(null, take());
    flushSync();

    expect(document.querySelector('.tc'), 'the record still draws the take').not.toBeNull();
    expect(
      document.querySelector('.tc .fr'),
      'a take this page did not capture has no count here',
    ).toBeNull();
  });

  it("lands a take's words at the caret and takes one green beat before easing back", async () => {
    vi.useFakeTimers();
    try {
      const harness = open({ dictation: true });
      type('fix the');

      // The take has to be one this composer began: the words a notice
      // carries are the take's, and only a take this page started is its
      // own to put in the box.
      await beginTake(harness);

      harness.page.record = record({
        composer: {
          take: null,
          notice: { kind: 'landed', text: 'flaky retry test', truncated: false },
          machine: null,
        },
      });
      flushSync();

      expect(field().value, 'the words land where the reader was about to type').toBe(
        'fix the flaky retry test',
      );
      expect(document.querySelector('.surf')?.classList.contains('done'), 'one green beat').toBe(
        true,
      );

      vi.advanceTimersByTime(1000);
      flushSync();

      expect(
        document.querySelector('.surf')?.classList.contains('done'),
        "and the border eases back to the box's own",
      ).toBe(false);
      expect(field().value, 'the words stay').toBe('fix the flaky retry test');
    } finally {
      vi.useRealTimers();
    }
  });

  /**
   * The keyboard comes with the words.
   *
   * Ved's report: a take lands, he presses Enter straight away, and nothing
   * sends - the key went to whatever still held the focus, so he had to click
   * back into the box first.
   */
  it('moves the keyboard to the box a take landed in, so an immediate Enter sends', async () => {
    const harness = open({ dictation: true });
    expect(document.activeElement, 'the field starts with the keyboard').toBe(field());

    const mic = document.querySelector('.mic');
    if (!(mic instanceof HTMLElement)) throw new Error('the box drew no mic');
    mic.focus();
    flushSync();
    expect(
      document.activeElement,
      'the mic took it, which is the state the take then lands into',
    ).not.toBe(field());

    // A take this page began, which is what makes the landing below the thing
    // that runs: without it the field's own remount brings the keyboard back
    // and the landing asserts nothing.
    await beginTake(harness);
    harness.page.record = record({
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'push it once CI is green', truncated: false },
        machine: null,
      },
    });
    flushSync();

    expect(field().value, 'the words landed').toBe('push it once CI is green');
    expect(
      document.activeElement,
      'and the take brings it back, so an immediate Enter submits',
    ).toBe(field());
  });

  /**
   * **The browser takeover owns the keyboard while it is up.** Its bar takes
   * it on open (#2571), and the composer's own reclaim must not pull it back
   * - the phone's typing once landed in the chat behind the picture
   * (measured on the dev stack).
   */
  it('leaves the keyboard to the browser takeover while it is up', () => {
    open({});
    expect(document.activeElement, 'the box owns it while no takeover').toBe(field());

    surface.phase = 'live';
    flushSync();
    const active = document.activeElement;
    expect(
      active instanceof HTMLElement &&
        document.querySelector('.takeover-bar')?.contains(active) === true,
      'the takeover owns the keyboard while it is up',
    ).toBe(true);
    expect(document.activeElement, 'and the box behind it does not').not.toBe(field());

    surface.phase = 'closed';
    flushSync();
    expect(document.activeElement, 'and the box takes it back when it lowers').toBe(field());
  });

  /**
   * **The box behind the picture is inert** (#2571): the bar owns the
   * keyboard while the surface is up, so the composer's own markup must say
   * so even before the bar's mount walk runs - the server render carries it.
   */
  it('marks the composer inert while the takeover is up', async () => {
    await surfaceOn(SLOT, 2);
    open({ record: record({ pending_asks: [browserHandOffAsk()] }) });
    flushSync();
    const box = document.querySelector('.surf');
    expect(box?.hasAttribute('inert'), 'the composer behind the picture is inert').toBe(true);
    surface.phase = 'closed';
    flushSync();
    expect(box?.hasAttribute('inert'), 'and it is given back with the chat').toBe(false);
  });

  /**
   * **The box is the keyboard's** (Ved, 2026-10-15): the first printable key
   * with nothing focused lands in the composer, and Tab never walks the pane -
   * it comes to the box from outside and cycles the box's own controls once
   * it is there, so a reader typing never watches the focus wander.
   */
  it('takes the first typed key and keeps Tab inside the box', () => {
    open({});
    // jsdom measures no layout, so every element's offsetParent is null: the
    // shim is what lets the visibility filter see the box's controls.
    const sheet = Object.getOwnPropertyDescriptor(HTMLElement.prototype, 'offsetParent');
    Object.defineProperty(HTMLElement.prototype, 'offsetParent', {
      configurable: true,
      get(this: HTMLElement) {
        return this.parentElement;
      },
    });
    try {
      (document.activeElement as HTMLElement | null)?.blur();
      window.dispatchEvent(
        new KeyboardEvent('keydown', { code: 'KeyA', key: 'a', bubbles: true, cancelable: true }),
      );
      flushSync();
      expect(field().value, 'the first key lands in the box').toBe('a');

      const outside = document.createElement('button');
      document.body.append(outside);
      outside.focus();
      flushSync();
      window.dispatchEvent(
        new KeyboardEvent('keydown', { code: 'Tab', key: 'Tab', bubbles: true, cancelable: true }),
      );
      flushSync();
      expect(document.activeElement, 'Tab from the pane comes to the box').toBe(field());

      window.dispatchEvent(
        new KeyboardEvent('keydown', { code: 'Tab', key: 'Tab', bubbles: true, cancelable: true }),
      );
      flushSync();
      const at = document.activeElement;
      expect(
        at instanceof HTMLElement && document.querySelector('.comp')?.contains(at) === true,
        'and from the field it cycles the box, never the pane',
      ).toBe(true);
      expect(at, 'off the field, onto one of its controls').not.toBe(field());
      outside.remove();
    } finally {
      if (sheet !== undefined) Object.defineProperty(HTMLElement.prototype, 'offsetParent', sheet);
    }
  });

  /**
   * **The landing itself is what moves the keyboard, when it has somewhere to
   * move it to.** The case above lands while the field is unmounted - the take
   * card has its row - so the field mounts back afterwards and its own mount
   * takes the caret, which leaves the landing's focus untested. A take whose
   * field is back in the row before its words land (the quick release the
   * arm-at-press fix was written for) is the shape that reaches it.
   */
  it('brings the keyboard to a field the landing found already drawn', async () => {
    const harness = open({ dictation: true });

    // A press and a release: the take is over on this side before its record
    // ever shows one, so the field is drawn again by the time the words land.
    key('ControlRight', 'keydown');
    await opened();
    key('ControlRight', 'keyup');
    endTake(harness);
    flushSync();
    expect(document.querySelector('.line textarea'), 'the field is back in the row').not.toBeNull();

    // The caret is somewhere else, as it is after a click away.
    field().blur();
    flushSync();
    expect(document.activeElement, 'and the caret is not in the box').not.toBe(field());

    harness.page.record = withNotice(LANDED);
    flushSync();

    expect(field().value, 'the words land').toBe(LANDED.text);
    expect(document.activeElement, 'and the landing brings the keyboard to them').toBe(field());
  });

  /**
   * A landing this page never took must not be placed. The words are the
   * reader's own, already sent, and the box they would come back in is not a
   * draft: it is the message they wrote, handed to them to send twice. Take
   * news reaches the client that began the take alone, so this case is the
   * safety for a page handed a notice it never took.
   */
  it('opens empty over a landed notice the server was already holding', () => {
    open({
      record: record({
        composer: {
          take: null,
          notice: {
            kind: 'landed',
            text: 'But where are we on the rate limiting side on our end?',
            truncated: false,
          },
          machine: null,
        },
      }),
    });

    expect(field().value, 'the box owes the reader nothing back').toBe('');
    expect(drawn(), 'and the notice draws no row of its own').not.toContain('rate limiting');
  });

  /**
   * A capped take is the one whose words are most likely still unsent, and its
   * row says to carry on from the end. Drawn on a client that never saw the
   * take, that is a note about words that are not there.
   */
  it("does not draw a truncated take's row on a client that never saw it", () => {
    open({
      record: record({
        composer: {
          take: null,
          notice: { kind: 'landed', text: 'this is what fitted', truncated: true },
          machine: null,
        },
      }),
    });

    expect(field().value, 'the words did not land here').toBe('');
    expect(document.querySelector('.notice.warn'), 'and nothing says they were cut').toBeNull();
  });

  it("lands a truncated take's words and still says the take was cut", async () => {
    const harness = open({ dictation: true });
    await beginTake(harness);

    harness.page.record = record({
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'this is what fitted', truncated: true },
        machine: null,
      },
    });
    flushSync();

    expect(field().value, 'the words land').toBe('this is what fitted');
    expect(
      document.querySelector('.notice.warn')?.textContent,
      'and the row says the take was cut rather than that they stopped speaking',
    ).toBe(TRUNCATED);

    // The page hands the same landing over again, as every frame does. The
    // arm is spent by now, so the row has to read the words this box holds
    // rather than the arm alone: reading the arm, it vanished on this
    // hand-over with the words still in the draft (the gate's failure axis,
    // 2026-10-15).
    harness.page.record = record({
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'this is what fitted', truncated: true },
        machine: null,
      },
    });
    flushSync();

    expect(field().value, 'the words are still there').toBe('this is what fitted');
    expect(
      document.querySelector('.notice.warn')?.textContent,
      'and the row still says the take was cut on the hand-over after the landing',
    ).toBe(TRUNCATED);
  });

  /**
   * The dock takes the box while a prompt waits, and it says what happens to a
   * take that lands behind it. That is the path the notice exists for, and the
   * words still have to be there when the box comes back.
   */
  it('holds a take that lands while a prompt has the box, and lands its words when the box returns', async () => {
    const harness = open({ dictation: true });
    type('ship it once CI is green');

    // **The take starts in the composer's own box** - that is the moment its
    // destination is captured - and the prompt arrives mid-flight, which does
    // not take words that were spoken for the draft the reader was in.
    await beginTake(harness);

    const watching = record({
      pending_asks: [permissionAsk()],
      composer: { take: take(), notice: null, machine: null },
    });
    harness.page.record = watching;
    flushSync();

    expect(document.querySelector('.dock'), 'the prompt has the box').not.toBeNull();

    harness.page.record = record({
      pending_asks: [permissionAsk()],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'push it once CI is green', truncated: false },
        machine: null,
      },
    });
    flushSync();

    harness.page.record = record();
    flushSync();

    expect(field().value, 'the take landed in the draft the box was holding').toBe(
      'ship it once CI is green push it once CI is green',
    );
  });

  it('draws the notice a take left instead of a row', () => {
    const harness = open();
    harness.page.record = record({
      composer: {
        take: null,
        notice: {
          kind: 'line',
          tone: 'q',
          text: 'nothing above -50 dBFS in 4s · loudest was -38.2 · try again',
        },
        machine: null,
      },
    });
    flushSync();

    expect(drawn(), 'what went wrong is the row the reader gets').toContain(
      'nothing above -50 dBFS in 4s',
    );
    expect(document.querySelector('.notice.q')).not.toBeNull();
    expect(document.querySelector('.dict'), 'a notice wins the row').toBeNull();
  });

  it('hints in the terminal own words, three dots and all', () => {
    // The placeholder is the whole of what the box says while it is empty, and
    // it was the one of the three lines nothing pinned: reverting it to the
    // single ellipsis character left every other test green.
    open({});

    expect(field().placeholder).toBe('Type a message...');
  });

  it('replaces the box entirely for each reason it cannot take keys, and says why', () => {
    const cases: [Partial<ComposerProps>, string, string | null][] = [
      [{ seat: seatRead({ lifecycle: 'Spawning' }) }, 'Waking up agent...', null],
      [{ seat: seatRead({ lifecycle: 'Sleeping' }) }, 'Waking up agent...', null],
      [
        {
          record: record({
            composer: { take: null, notice: null, machine: 'compacting' },
          }),
        },
        'compacting this session',
        null,
      ],
      [
        {
          record: record({
            composer: { take: null, notice: null, machine: 'handoff' },
          }),
        },
        'handing this session over',
        null,
      ],
      [
        { seat: seatRead({ lifecycle: 'Failed', reason: 'the CLI exited with status 1' }) },
        'Input disabled due to error',
        'the CLI exited with status 1',
      ],
      [
        // The shape the session page hands over for a LEAD nothing has
        // started: no roster row, so no lifecycle and `waking` true together -
        // and the page's own open dispatches the spawn, so the line is the
        // wake rather than a refusal.
        {
          seat: seatRead({
            lifecycle: null,
            waking: true,
            reason: 'no session has been started here',
          }),
        },
        'Waking up agent...',
        null,
      ],
      [
        // A worker's seat with nothing behind it has no spawn coming - only
        // its lead can start it - so its line keeps saying so.
        {
          slot: { org: 'Busytools', project: 'forge', label: 'w1' },
          seat: seatRead({
            lifecycle: null,
            waking: true,
            reason: 'no session has been started here',
          }),
        },
        'not running',
        'no session has been started here',
      ],
    ];

    for (const [props, line, sub] of cases) {
      open(props);
      expect(drawn(), `the slot says why: ${line}`).toContain(line);
      if (sub !== null) expect(drawn(), 'and what to do about it').toContain(sub);
      expect(document.querySelector('textarea'), 'a blocked box takes no keys').toBeNull();
      void unmount(app as Record<string, unknown>);
      app = null;
      document.body.innerHTML = '';
    }
  });

  /**
   * **Every narrative hold draws the same way: one band, the sweep, only the
   * text changing.** The selector is the pin - a pin on the line alone would
   * survive the animation being dropped, which is how it goes missing. The
   * error state shares the band in its own tone, and the worker's stop draws
   * no motion at all, because neither is progress.
   */
  it('draws every narrative hold with the one sweep, and no motion on a stop', () => {
    const cases: [Partial<ComposerProps>, string][] = [
      [{ seat: seatRead({ lifecycle: 'Spawning' }) }, 'Waking up agent...'],
      [{ seat: seatRead({ waking: true, lifecycle: null, reason: null }) }, 'Waking up agent...'],
      [
        {
          record: record({
            composer: { take: null, notice: null, machine: 'compacting' },
          }),
        },
        'compacting this session',
      ],
      [
        {
          record: record({
            composer: { take: null, notice: null, machine: 'handoff' },
          }),
        },
        'handing this session over',
      ],
    ];
    for (const [props, line] of cases) {
      open(props);
      expect(drawn(), `the band says ${line}`).toContain(line);
      expect(
        document.querySelector('.blocked .b1 .shimmer')?.textContent,
        `${line} lost the sweep`,
      ).toBe(line);
      void unmount(app as Record<string, unknown>);
      app = null;
      document.body.innerHTML = '';
    }

    // The error: the same band, its own tone, no motion.
    open({ seat: seatRead({ lifecycle: 'Failed', reason: 'the CLI exited with status 1' }) });
    expect(drawn(), 'the error line moved').toContain('Input disabled due to error');
    expect(document.querySelector('.blocked.err'), 'the error lost its tone').not.toBeNull();
    expect(document.querySelector('.blocked .shimmer'), 'the error is not progress').toBeNull();

    // The worker's stop: a dead seat, said plainly.
    void unmount(app as Record<string, unknown>);
    app = null;
    document.body.innerHTML = '';
    open({
      slot: { org: 'Busytools', project: 'forge', label: 'w1' },
      seat: seatRead({ waking: true, lifecycle: null, reason: null }),
    });
    expect(drawn(), 'the stop line moved').toContain('not running');
    expect(document.querySelector('.blocked .shimmer'), 'a stop is not progress').toBeNull();
  });

  /**
   * **A machine turn takes the box and hands it back with the draft.** The
   * handoff and a compaction are turns the person cannot type into, so the
   * box stands down while they run - and the words already typed are the
   * reader's, so they come back with the box rather than going with the turn.
   */
  it('keeps a draft through a machine turn and hands it back after', () => {
    const harness = open({ dictation: true });
    type('the next thing I want to say');
    expect(document.querySelector('.mic'), 'the control is there before the turn').not.toBeNull();

    for (const [machine, line] of [
      ['handoff', 'handing this session over'],
      ['compacting', 'compacting this session'],
    ] as const) {
      harness.page.record = record({
        composer: { take: null, notice: null, machine },
      });
      flushSync();
      expect(drawn(), 'the composer says what is running').toContain(line);
      expect(document.querySelector('textarea'), 'a machine turn takes the box').toBeNull();
      expect(document.querySelector('.send'), 'and the send').toBeNull();
      expect(document.querySelector('.mic'), 'and the mic').toBeNull();
      expect(document.querySelector('.b1 .shimmer'), 'and it draws the sweep').not.toBeNull();
      expect(document.querySelector('.b1 .ring'), 'and no second animation beside it').toBeNull();
    }

    harness.page.record = record({
      composer: { take: null, notice: null, machine: null },
    });
    flushSync();
    expect(field().value, "the reader's words come back with the box").toBe(
      'the next thing I want to say',
    );
  });

  /**
   * **A parked ask outranks the machine blocker.** Both machine turns are
   * ordinary tool-using turns - the judgment turn writes memory files and
   * stops processes - so they raise permission asks, and this composer is the
   * only surface that answers them. A machine line drawn over one would wedge
   * the seat: the dock could not draw, and the CLI would wait for an answer
   * nobody can give for ever.
   */
  /**
   * **The bound key opens nothing under a machine turn.** The box draws no
   * mic and no field there, so the absence of a control proves nothing: the
   * key is a window listener, and without the gate it would open the capture
   * and land the words in a draft nobody can see.
   */
  it('opens no capture from the bound key while a machine turn owns the box', async () => {
    // **The control first.** An empty send below proves nothing unless the
    // same press begins a take on a live box - the key is a window listener,
    // and a misspelt code would read exactly like the gate working.
    const live = open({ dictation: true });
    key('ControlRight', 'keydown');
    await opened();
    expect(live.sent, 'the control: a live box begins the take').toHaveLength(1);
    void unmount(app as Record<string, unknown>);
    app = null;
    document.body.innerHTML = '';

    const harness = open({
      dictation: true,
      record: record({
        composer: { take: null, notice: null, machine: 'handoff' },
      }),
    });

    key('ControlRight', 'keydown');
    // The same turns the control gave the open, so an empty send means the
    // gate refused rather than the promise not having settled.
    await opened();

    expect(harness.sent, 'the key begins nothing under a machine turn').toEqual([]);
  });

  it('draws and answers a parked ask through a machine turn', () => {
    const harness = open({
      record: record({
        composer: { take: null, notice: null, machine: 'handoff' },
        pending_asks: [permissionAsk()],
      }),
    });

    expect(drawn(), 'the ask is what the slot draws').toContain('Allow once');
    expect(drawn(), 'and the machine line stays out of its way').not.toContain(
      'handing this session over',
    );

    const allow = [...document.querySelectorAll('.dock .acts .btn')].find((button) =>
      button.textContent?.includes('Allow once'),
    );
    if (!(allow instanceof HTMLElement)) throw new Error('the dock drew no allow');
    allow.click();
    flushSync();

    const [sent] = harness.sent;
    expect(sent?.command['respond_permission'], 'the answer reaches the core').toMatchObject({
      key: { org: 'Busytools', project: 'forge', label: 'lead' },
      tool_id: 'tu-1',
      outcome: { outcome: 'selected', option_id: 'opt-once' },
    });
  });

  /**
   * The waking line carries its own motion: a sweep through the words rather
   * than the ring, because a ring beside a moving line is two animations
   * saying one thing. The loads guidance's shape - a gradient sweep reads as
   * loading more clearly than a pulse, and one loop is the whole of it.
   */
  it('draws the waking line as a sweep, with no second animation beside it', () => {
    open({ seat: seatRead({ lifecycle: 'Spawning' }) });

    expect(drawn(), 'the box lost the waking line').toContain('Waking up agent...');
    expect(document.querySelector('.b1 .shimmer'), 'the waking line does not sweep').not.toBeNull();
    expect(
      document.querySelector('.b1 .ring'),
      'the waking line draws a ring beside its own motion',
    ).toBeNull();

    // The lead's own wake arm carries the same pair: an edit that left the
    // sweep on the spawn arm alone would keep everything above green.
    void unmount(app as Record<string, unknown>);
    app = null;
    document.body.innerHTML = '';
    open({ seat: seatRead({ lifecycle: null, waking: true, reason: null }) });
    expect(document.querySelector('.b1 .shimmer'), "the lead's wake lost its sweep").not.toBeNull();
    expect(document.querySelector('.b1 .ring'), 'a ring joined the lead wake').toBeNull();
  });

  it('takes the mark off a send the core has started, and keeps the words', () => {
    const harness = open();
    type('push it once CI is green');
    press('Enter');
    expect(echoAt(SLOT)?.state, 'the send is on its way').toBe('sending');

    harness.page.record = record({ header: { turn_in_flight: true } });
    flushSync();

    // The row is the reader's message from here on: the mark goes and the words
    // stay, because the core's own copy of them does not arrive as a frame - a
    // prompt forge injects is not echoed on stream-json - so a row that went
    // with the mark would take the words off screen for the length of the turn.
    expect(echoAt(SLOT), 'the words stay where they were sent from').toMatchObject({
      state: 'taken',
      words: 'push it once CI is green',
    });
  });

  /**
   * A send into a turn that was already running did not start it.
   *
   * `turn_in_flight` says the core has a turn, not that it has THIS send. On a
   * seat already running, the take effect fired the moment the echo was posted
   * and flipped it straight to `taken` - and `refuse` rewrites only a send
   * still on its way, so the server's refusal landed nowhere: the words stayed
   * drawn with no mark, no reason and no way to send them again.
   */
  it('keeps a mid-turn send on its way, so a refusal can land on it', () => {
    const harness = open({ record: record({ header: { turn_in_flight: true } }) });
    type('run the gate again');
    sendBox();

    expect(echoAt(SLOT)?.state, 'the turn was already running, so this send did not start it').toBe(
      'sending',
    );

    harness.say({ kind: 'error', what: 'dispatch', why: 'the session is not running' });
    flushSync();

    expect(echoAt(SLOT), 'and the refusal has somewhere to land').toMatchObject({
      state: 'failed',
      words: 'run the gate again',
      why: 'the session is not running',
    });
  });

  /**
   * **A send reads the seat's state as it is now, not as the page last drew
   * it.** The record this box draws is written once per painted frame, so the
   * frame that opened a turn can be on the seat and not yet in the record -
   * and a send read off the record posts as "no turn was running", is taken by
   * the very publish that carries the turn, and loses the refusal that comes
   * for it, which a taken send can no longer be given.
   */
  it('keeps a send on its way for a turn applied but not drawn yet', () => {
    const on = wire();
    // The turn is on the seat already...
    on.hold(SLOT, { chat_appended: { key: SLOT, msg: openTurn() } });
    // ...while the record this box draws has not been written with it.
    const harness = open({ record: record({ header: { turn_in_flight: false } }) }, on);
    type('run the gate again');
    sendBox();

    // The paint that carries the turn arrives.
    harness.page.record = record({ header: { turn_in_flight: true } });
    flushSync();

    expect(
      echoAt(SLOT)?.state,
      'a send into the turn already on the seat was taken by the paint carrying it',
    ).toBe('sending');
  });

  /**
   * The record in hand can be another seat's - the page keeps this composer
   * mounted across a switch - and its turn says nothing about this seat.
   */
  it("does not take this seat's send with another seat's record in hand", () => {
    const harness = open();
    type('for the seat I am on');
    sendBox();
    expect(echoAt(SLOT)?.state, 'on its way').toBe('sending');

    harness.page.record = record({ slot: ELSEWHERE, header: { turn_in_flight: true } });
    flushSync();

    expect(echoAt(SLOT)?.state, "another seat's turn is not this send being taken").toBe('sending');
  });

  /**
   * A send the reader walked away from is still theirs.
   *
   * Leaving a seat used to drop every other seat's send still on its way, so a
   * refusal that arrived after the switch was heard by nobody: the row, the
   * reason and the way to send it again all went with the echo.
   */
  it('keeps a send the reader left, so a refusal still finds it', () => {
    const harness = open();
    type('send it before I move on');
    sendBox();

    harness.page.slot = ELSEWHERE;
    harness.page.record = record({ slot: ELSEWHERE });
    flushSync();

    harness.say({ kind: 'error', what: 'dispatch', why: 'the session is not running' });
    flushSync();

    expect(echoAt(SLOT), 'the send the reader left behind still hears the refusal').toMatchObject({
      state: 'failed',
      words: 'send it before I move on',
      why: 'the session is not running',
    });
  });

  /**
   * A seat switch leaves the keyboard in the box, which is the page's resting
   * focus (#1669): nothing on a switch steals the caret. The composer stays
   * mounted across a switch, so the field a reader was typing in is the field
   * they are typing in still - and that is exactly what this pins, that no
   * effect keyed on the seat grabs the keyboard or lets it go. A remount would
   * re-run the mount's own focus effect and the caret would never reach the
   * body, which is the draft tests' ground rather than this one's.
   */
  it('leaves the keyboard in the box across a seat switch', () => {
    const harness = open();
    expect(document.activeElement, 'the box starts with the keyboard').toBe(field());

    harness.page.slot = ELSEWHERE;
    harness.page.record = record({ slot: ELSEWHERE });
    flushSync();

    expect(document.activeElement, 'and still has it after the switch').toBe(field());
  });

  it('names the slash command a turn is still working on', () => {
    const harness = open();
    type('/compact');
    // Enter takes the row the list is offering - the command itself - and the
    // next one sends it, which is the order a reader's hands move in.
    press('Enter');
    press('Enter');
    flushSync();

    harness.page.record = record({ header: { turn_in_flight: true } });
    flushSync();

    expect(drawn(), 'the reader is told what they are waiting for').toContain('Running /compact');
    expect(
      document.querySelector('.blocked .b1 .shimmer')?.textContent,
      'the command line lost the same sweep every hold draws',
    ).toBe('Running /compact');
  });

  it('rests as one row when it has nothing to put in a footer', () => {
    open();

    expect(
      document.querySelector('.foot'),
      'a box with no controls and no draft drew a footer row',
    ).toBeNull();
  });

  /**
   * **An idle bar is one row, whatever else the seat is doing** (Ved,
   * 2026-10-09), so the controls ride the field's own line.
   *
   * The gate was `filled || dictation || running`, which made a seat with a
   * live turn two rows tall for a stop button nobody was looking at. **Both
   * arms of the new gate are pinned by nothing else**: reverting it whole, or
   * keeping `filled` alone, leaves every other test in this file green.
   */
  it('rests as one row with its controls on the field, draftless with a turn running', () => {
    open({ dictation: true, record: record({ header: { turn_in_flight: true } }) });

    expect(document.querySelector('.foot'), 'an idle bar drew a second row').toBeNull();
    expect(document.querySelector('.line .mic'), 'the mic left the field line').not.toBeNull();
    expect(document.querySelector('.line .stop'), 'the stop left the field line').not.toBeNull();
  });

  /** The other arm: a recording takes the draft's own line, controls on it. */
  it("gives a recording the draft's own line, with the controls on it", () => {
    const harness = open({ dictation: true });
    harness.page.record = record({
      composer: { take: take(), notice: null, machine: null },
    });
    flushSync();

    expect(document.querySelector('.line .tc'), 'the take took no row').not.toBeNull();
    expect(
      document.querySelector('.line .mic'),
      'the mic did not stay on the row the take runs in',
    ).not.toBeNull();
    expect(document.querySelector('.foot'), 'a recording grew a second row').toBeNull();
  });

  /**
   * **The mic keeps the caret when a started take redraws it.**
   *
   * The gate destroys and rebuilds the button as the take takes the row, so a
   * reader who pressed the mic from the keyboard would lose the caret to the
   * body - Enter could no longer finish the take.
   */
  it('keeps the caret on the mic when a started take redraws its row', () => {
    const harness = open({ dictation: true });
    const resting = document.querySelector('.line .mic');
    expect(resting, 'precondition: the mic rests on the field line').not.toBeNull();

    (resting as HTMLElement).focus();
    (resting as HTMLElement).click();
    harness.page.record = record({
      composer: { take: take(), notice: null, machine: null },
    });
    flushSync();

    expect(
      document.querySelector('.line .mic'),
      'precondition: the take redrew the mic for its row',
    ).not.toBeNull();
    expect(document.activeElement?.className, 'the caret fell off the mic').toContain('mic');
  });

  /**
   * **The body is one row, so a draft never grows a second one.** The controls
   * ride the field's own line at every width; the keys are a pointer's detail
   * on that same line. A foot returning here is the band growing for a draft,
   * which is the height this cut removes.
   */
  it('keeps the controls on the field row, so a draft grows no second row', () => {
    open({ dictation: true, record: record({ header: { turn_in_flight: true } }) });
    type('a draft');

    for (const control of ['.line .mic', '.line .stop', '.line .send']) {
      expect(
        document.querySelector(control),
        `${control} left the field's line for a row of its own`,
      ).not.toBeNull();
    }
    expect(
      document.querySelector('.foot'),
      'the band grew a second row under the draft',
    ).toBeNull();
  });

  /**
   * The field's row draws for a draft alone: an install that cannot dictate
   * has no mic, and a gate tightened to `dictation || running` would take the
   * send and both hints with it - the reachable case, since the router makes
   * dictation false whenever the wire omits it.
   */
  it('draws the send and the keys for a draft alone, with no dictation', () => {
    open();
    type('a draft');

    expect(document.querySelector('.line .send'), 'a draft drew no way to send it').not.toBeNull();
    expect(document.querySelector('.line .hints'), 'the keys went with the mic').not.toBeNull();
    expect(document.querySelector('.mic'), 'an install that cannot dictate drew a mic').toBeNull();
  });

  it('offers the way in only when this install can dictate, and a press is the trigger', async () => {
    open();
    expect(
      document.querySelector('.mic'),
      'a control it cannot honour is worse than none',
    ).toBeNull();
    void unmount(app as Record<string, unknown>);
    app = null;
    document.body.innerHTML = '';

    const harness = open({ dictation: true });
    const mic = document.querySelector('.mic');
    if (!(mic instanceof HTMLElement))
      throw new Error('an install that can dictate draws no way in');
    vi.useFakeTimers();
    try {
      // The press as a finger delivers it: down, up, then the click - with the
      // release cancelling the hold, so advancing past the threshold after it
      // must open nothing.
      mic.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true }));
      vi.advanceTimersByTime(100);
      mic.dispatchEvent(new MouseEvent('pointerup', { bubbles: true }));
      mic.click();
      vi.advanceTimersByTime(400);
    } finally {
      vi.useRealTimers();
    }
    for (let turn = 0; turn < 4; turn += 1) await Promise.resolve();
    flushSync();

    expect(harness.sent.at(-1)?.command['dictate_stream'], 'a press is the trigger').toBeDefined();
    expect(harness.sent, 'and only the one take began').toHaveLength(1);
    expect(document.querySelector('.pop'), 'a quick press opens no panel').toBeNull();
  });

  it('opens the settings on a hold, and the release starts nothing', async () => {
    vi.useFakeTimers();
    try {
      const harness = open({ dictation: true });
      const mic = document.querySelector('.mic');
      if (!(mic instanceof HTMLElement))
        throw new Error('an install that can dictate draws no way in');

      mic.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true }));
      vi.advanceTimersByTime(450);
      for (let turn = 0; turn < 4; turn += 1) await Promise.resolve();
      flushSync();

      expect(document.querySelector('.pop'), 'the hold is the door').not.toBeNull();
      expect(harness.sent, 'and the hold starts no take').toEqual([]);

      mic.dispatchEvent(new MouseEvent('pointerup', { bubbles: true }));
      mic.click();
      for (let turn = 0; turn < 4; turn += 1) await Promise.resolve();
      flushSync();
      expect(harness.sent, 'the release does not read as a press').toEqual([]);
      expect(document.querySelector('.pop'), 'and the settings stay open').not.toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("re-anchors the swallow at the release, so a long hold's click is not a press", async () => {
    vi.useFakeTimers();
    try {
      const harness = open({ dictation: true });
      const mic = document.querySelector('.mic');
      if (!(mic instanceof HTMLElement))
        throw new Error('an install that can dictate draws no way in');

      // A slow hold: past the threshold AND past the swallow's window measured
      // from the fire - the release's click is still the hold's, not a press.
      mic.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true }));
      vi.advanceTimersByTime(450);
      vi.advanceTimersByTime(900);
      mic.dispatchEvent(new MouseEvent('pointerup', { bubbles: true }));
      mic.click();
      for (let turn = 0; turn < 4; turn += 1) await Promise.resolve();
      flushSync();

      expect(document.querySelector('.pop'), 'the hold is still the door').not.toBeNull();
      expect(harness.sent, 'a long hold does not start a take on release').toEqual([]);
    } finally {
      vi.useRealTimers();
    }
  });

  it('expires the swallow, so a hold with no trailing click does not eat the next press', async () => {
    vi.useFakeTimers();
    try {
      const harness = open({ dictation: true });
      const mic = document.querySelector('.mic');
      if (!(mic instanceof HTMLElement))
        throw new Error('an install that can dictate draws no way in');

      // A hold whose click never arrives (a drag off, or a touch long-press):
      // the swallow must lapse, or the NEXT activation silently does nothing.
      mic.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true }));
      vi.advanceTimersByTime(450);
      mic.dispatchEvent(new MouseEvent('pointerup', { bubbles: true }));
      vi.advanceTimersByTime(900);
      mic.click();
      for (let turn = 0; turn < 4; turn += 1) await Promise.resolve();
      flushSync();

      expect(
        harness.sent.at(-1)?.command['dictate_stream'],
        'a stale swallow ate the next press',
      ).toBeDefined();
    } finally {
      vi.useRealTimers();
    }
  });

  it('opens the settings from the keyboard door, with no take', () => {
    const harness = open({ dictation: true });
    const mic = document.querySelector('.mic');
    if (!(mic instanceof HTMLElement))
      throw new Error('an install that can dictate draws no way in');

    mic.dispatchEvent(
      new KeyboardEvent('keydown', {
        key: 'Enter',
        shiftKey: true,
        bubbles: true,
        cancelable: true,
      }),
    );
    flushSync();

    expect(document.querySelector('.pop'), 'Shift+Enter is the hold for a keyboard').not.toBeNull();
    expect(harness.sent, 'and starting nothing').toEqual([]);
  });
});

/**
 * The push-to-talk key, which starts a take from anywhere in the box.
 *
 * The binding and the mode are read off the record rather than assumed: they
 * are the user's `forge.toml`, and a page that hardcoded a chord would honour a
 * different key on every install that moved it. `dictate-key.test.ts` pins what
 * a press MEANS; this pins what the box DOES with it.
 */
describe('the key', () => {
  /** A record whose composer carries the binding and the mode under test. */
  /** The seat's own storage key, which the page reads its pick and default by. */
  const SEAT = subjectKey({ session: SLOT });

  const bound = (bind: string, mode: string, take: Record<string, unknown> | null = null) =>
    record({
      composer: { take, notice: null, machine: null, bind, mode },
    });

  it('brings the keyboard to the box before the take starts', async () => {
    const harness = open({ dictation: true });
    harness.page.record = bound('right_cmd', 'auto');
    flushSync();

    // The reader's hands are elsewhere - a rail link, the mic button, the body
    // - and the shortcut is meant to move the focus to the box first, so the
    // transcript lands where the keys would (#1669's third transition).
    field().blur();
    expect(document.activeElement, 'the control starts with the caret off the box').not.toBe(
      field(),
    );

    key('ControlRight', 'keydown');
    expect(document.activeElement, 'the start brings the keyboard to the box').toBe(field());
    await opened();
    expect(harness.sent, 'and only then starts listening').toHaveLength(1);
  });

  it('starts a take on the bound key, and transcribes it when the key is held', async () => {
    vi.useFakeTimers();
    try {
      const harness = open({ dictation: true });
      harness.page.record = bound('right_cmd', 'auto');
      flushSync();

      key('ControlRight', 'keydown');
      await opened();
      expect(harness.sent, 'the key is how a take begins').toEqual([
        {
          command: {
            dictate_stream: {
              key: { org: 'Busytools', project: 'forge', label: 'lead' },
              options: { styling: 'semi_formal', structure: 'prose', context: 'general' },
              surface: 'composer',
            },
          },
        },
      ]);

      vi.advanceTimersByTime(500);
      key('ControlRight', 'keyup');
      settle();
      expect(harness.sent.at(-1)?.command, 'a hold released transcribes what was said').toEqual({
        dictate_stop: { key: { org: 'Busytools', project: 'forge', label: 'lead' }, submit: true },
      });
    } finally {
      vi.useRealTimers();
    }
  });

  /**
   * The states a take passes through, named in order.
   *
   * This is the lie Ved reported: the row said recording while the microphone
   * was still warming up, so the first words were spoken into a take that
   * already claimed to be keeping them. **The open is the band's own state
   * now, and adds no line** - the words read here are its status line for a
   * screen reader, which is the state said rather than drawn - and then the
   * row says `ready` until a frame carries a real sample, which is where
   * recording actually begins.
   */
  it('names the open, says ready, and only calls it recording on real audio', async () => {
    mic.held.gated = true;
    try {
      const harness = open({ dictation: true });
      harness.page.record = bound('right_cmd', 'auto');
      flushSync();

      key('ControlRight', 'keydown');
      flushSync();
      expect(
        document.querySelector('.tc'),
        'no row claims a take that does not exist yet',
      ).toBeNull();
      expect(
        document.querySelector('.comp .slot > .notice'),
        'and the press writes no line into the composer',
      ).toBeNull();

      mic.held.release?.();
      await opened();
      expect(drawn(), 'the capture is open, and nothing has arrived yet').toContain('ready');
      expect(document.querySelector('.tc'), 'the row is up once there is a take').not.toBeNull();

      // A frame of digital silence is a device still waking, not a reader
      // speaking: the row must not call that recording.
      mic.held.onFrame?.(new Uint8Array(641));
      flushSync();
      expect(drawn(), 'silence is not speech').toContain('ready');
      expect(document.querySelector('.tc .wave'), 'and there is no audio to draw').toBeNull();

      mic.held.onFrame?.(encodeFrame([0.5]));
      flushSync();
      expect(
        document.querySelector('.tc .wave'),
        'the first real sample starts the graph',
      ).not.toBeNull();
      expect(
        document.querySelector('.tc .sr')?.textContent,
        'and the state a screen reader hears says so',
      ).toBe('dictating');
    } finally {
      mic.held.gated = false;
      mic.held.release = null;
    }
  });

  /**
   * A capture the device is feeding nothing is said out loud, while it is
   * happening.
   *
   * A device layer that hands digital silence - a per-app mic mode, a privacy
   * toggle, another app on the microphone - reads `ready` for the whole take
   * and draws no graph, and until now the reader only learned it once the take
   * ended. The line is withdrawn the moment a sample lands, so it can never
   * stand over a take that is speaking.
   */
  it('says the capture is producing nothing once a take has run without signal', async () => {
    mic.held.gated = true;
    try {
      vi.useFakeTimers();
      const harness = open({ dictation: true });
      harness.page.record = bound('right_cmd', 'auto');
      flushSync();

      key('ControlRight', 'keydown');
      mic.held.release?.();
      await opened();
      mic.held.onFrame?.(new Uint8Array(641));

      vi.advanceTimersByTime(SILENT_NOTICE_MS - 1);
      flushSync();
      expect(document.querySelector('.tc .silent'), 'nothing is said inside the bound').toBeNull();

      vi.advanceTimersByTime(1);
      flushSync();
      expect(
        document.querySelector('.tc .silent')?.textContent,
        'the capture has produced nothing for seconds, so the take line says so',
      ).toContain('nothing is coming through it');

      mic.held.onFrame?.(encodeFrame([0.5]));
      flushSync();
      expect(
        document.querySelector('.tc .silent'),
        'and a real sample withdraws it at once',
      ).toBeNull();
    } finally {
      mic.held.gated = false;
      mic.held.release = null;
      vi.useRealTimers();
    }
  });

  /**
   * A release inside the bound is not the row's to speak into.
   *
   * The take is still non-null while it drains, and `flowing` is still false
   * on a take that never spoke, so the bound firing mid-drain would flash the
   * sentence over a take that is already ending.
   */
  it('never says it for a take released at the bound', async () => {
    mic.held.gated = true;
    try {
      vi.useFakeTimers();
      const harness = open({ dictation: true });
      harness.page.record = bound('right_cmd', 'auto');
      flushSync();

      key('ControlRight', 'keydown');
      mic.held.release?.();
      await opened();
      mic.held.onFrame?.(new Uint8Array(641));
      flushSync();

      vi.advanceTimersByTime(SILENT_NOTICE_MS - 100);
      key('ControlRight', 'keyup');
      flushSync();
      vi.advanceTimersByTime(100);
      flushSync();
      expect(
        document.querySelector('.tc .silent'),
        'the bound landing mid-drain must not say the capture is silent',
      ).toBeNull();
    } finally {
      mic.held.gated = false;
      mic.held.release = null;
      vi.useRealTimers();
    }
  });

  /**
   * What the guards together buy, rather than either one alone: the bound
   * never sets its flag over a take that spoke (the timer's check), and the
   * line never draws over a take that is speaking (its own check, which is
   * the one the sibling above has teeth on - it dies on that guard removed).
   * The property here is the reader's: a take with a voice in it is never
   * called silent, whichever of the two is holding it up.
   */
  it('never says it for a take that speaks inside the bound', async () => {
    mic.held.gated = true;
    try {
      vi.useFakeTimers();
      const harness = open({ dictation: true });
      harness.page.record = bound('right_cmd', 'auto');
      flushSync();

      key('ControlRight', 'keydown');
      mic.held.release?.();
      await opened();
      mic.held.onFrame?.(encodeFrame([0.5]));
      flushSync();

      vi.advanceTimersByTime(SILENT_NOTICE_MS * 4);
      flushSync();
      expect(
        document.querySelector('.tc .silent'),
        'a take with a voice in it is never called silent',
      ).toBeNull();
    } finally {
      mic.held.gated = false;
      mic.held.release = null;
      vi.useRealTimers();
    }
  });

  /**
   * A press inside the release's drain window is absorbed, not refused.
   *
   * The release keeps the capture open for a moment so the audio in flight
   * lands, and the take is still on this side until it does. The press has
   * nothing to start - the take it would refuse is the one the reader just
   * let go of - so it is a no-op rather than the "in use" line, which would
   * name the reader's own take back at them.
   */
  it('absorbs a press that lands while the release is draining', async () => {
    vi.useFakeTimers();
    try {
      const harness = open({ dictation: true });
      harness.page.record = bound('right_cmd', 'auto');
      flushSync();

      key('ControlRight', 'keydown');
      await opened();
      vi.advanceTimersByTime(500); // a hold, not a tap
      key('ControlRight', 'keyup'); // the release starts the drain
      flushSync();

      key('ControlRight', 'keydown'); // a press inside the window
      flushSync();
      expect(harness.sent, 'nothing new starts').toHaveLength(1);
      expect(drawn(), 'and nothing is refused by name').not.toContain('in use');

      settle();
      await Promise.resolve();
      expect(harness.sent.at(-1)?.command, 'and the take still stops once').toEqual({
        dictate_stop: { key: { org: 'Busytools', project: 'forge', label: 'lead' }, submit: true },
      });
    } finally {
      vi.useRealTimers();
    }
  });

  /**
   * **A take whose capture never opened takes its arm with it.** A refused
   * permission draws the line, and no start ever reaches the server - so the
   * record's notice never goes null, and an arm left standing was ridden by
   * the next landing on the seat, which is another client's take (the gate's
   * Important, 2026-10-15).
   */
  it('drops the arm of a take whose capture never opened', async () => {
    const harness = open({ dictation: true });
    mic.held.refuse = true;
    try {
      key('ControlRight', 'keydown');
      await opened();
      key('ControlRight', 'keyup');

      expect(drawn(), 'the failure draws its line').toContain('the microphone was refused');
      expect(harness.sent, 'and nothing reached the server, so no start ever comes').toEqual([]);

      harness.page.record = withNotice(LANDED);
      flushSync();

      expect(field().value, "the failed take's arm rode a landing that was not its own").toBe('');
    } finally {
      mic.held.refuse = false;
    }
  });

  /**
   * The press writes no line into the composer.
   *
   * Ved, in the #2159 look: "the layout of the composer shifts up and down
   * ... the text comes in between". The open is the band's own state now - a
   * colour that breathes, and a status line for a screen reader - so pressing
   * the key adds no row and moves nothing.
   */
  it('draws the open on the band only once it takes a moment', async () => {
    mic.held.gated = true;
    try {
      vi.useFakeTimers();
      const harness = open({ dictation: true });
      harness.page.record = bound('right_cmd', 'auto');
      flushSync();
      expect(document.querySelector('.comp .slot > .notice'), 'nothing is said yet').toBeNull();

      key('ControlRight', 'keydown');
      flushSync();
      expect(
        document.querySelector('.comp .surf')?.classList.contains('load'),
        'a press alone draws nothing: the desktop open is tens of milliseconds',
      ).toBe(false);

      // An open that outlasts the delay is a state worth drawing: a browser's
      // is the permission round trip and the audio graph's start.
      vi.advanceTimersByTime(500);
      flushSync();
      expect(
        document.querySelector('.comp .surf')?.classList.contains('load'),
        'an open that takes a moment says so on the band',
      ).toBe(true);
      expect(document.querySelector('.comp .sr')?.textContent, 'and a screen reader hears it').toBe(
        'opening the microphone',
      );
      expect(document.querySelector('.comp .slot > .notice'), 'and no line is added').toBeNull();

      mic.held.release?.();
      await opened();
      expect(
        document.querySelector('.comp .slot > .notice'),
        'and none once the open is over either',
      ).toBeNull();
    } finally {
      mic.held.gated = false;
      mic.held.release = null;
      vi.useRealTimers();
    }
  });

  /**
   * A record still transcribing is not what the band is about once this page
   * holds a take of its own.
   *
   * The record keeps the last take through its transcription, so it can still
   * be showing one while this page's next take is live - and the band must
   * follow this page's own take rather than the one the record is finishing
   * (Ved, in the look: "the color changes, and then goes back to orange").
   */
  it("keeps the band on this page's take while the record shows a transcribing one", async () => {
    const harness = open({ dictation: true });
    harness.page.record = bound('right_cmd', 'auto');
    flushSync();

    key('ControlRight', 'keydown');
    await opened();

    // The record now carries the previous take, still transcribing.
    harness.page.record = bound(
      'right_cmd',
      'auto',
      take({ phase: 'transcribing', progress: [1, 4] }),
    );
    flushSync();

    const surf = () => document.querySelector('.comp .surf');
    expect(surf()?.classList.contains('tr'), "the record's transcribing must not win").toBe(false);
    expect(surf()?.classList.contains('rec'), "the band is this page's own take").toBe(true);
  });

  /**
   * The card's counts move, rather than freezing at their first draw.
   *
   * The card draws them from the ring's own signals, so frames pushed while
   * the take runs keep moving it; a card stuck at its first draw would show
   * the first second of a thirty-second take, which is the reading the counts
   * exist to make impossible.
   */
  it('moves the card counts as the take produces frames', async () => {
    const shared = wire();
    const harness = open({ dictation: true }, shared);
    harness.page.record = bound('right_cmd', 'auto');
    flushSync();

    key('ControlRight', 'keydown');
    await opened();
    // The server's `dictate_started` is what puts the take on the record, and
    // with it the card this test reads.
    harness.page.record = bound('right_cmd', 'auto', take());
    flushSync();

    const card = () => document.querySelector('.tc')?.textContent ?? '';
    const frame = new Uint8Array(641);
    for (let at = 0; at < 12; at += 1) mic.held.onFrame?.(frame);
    flushSync();
    expect(card(), 'the count at the first draw').toContain('12 fr');
    expect(card(), 'and the bytes the socket took with them').toContain('7.5 KB');

    // The socket stops taking and the take keeps producing: the two halves
    // move apart, and the frames the socket did not take are named rather
    // than left to be inferred from the gap.
    shared.takes = false;
    for (let at = 0; at < 100; at += 1) mic.held.onFrame?.(frame);
    flushSync();
    expect(card(), 'the frames produced, a hundred of them later').toContain('112 fr');
    expect(card(), 'while the bytes taken stay where the socket left them').toContain('7.5 KB');
  });

  /**
   * The pace is a live reading, not the one the card opened on.
   *
   * It is computed from the ring's own samples when the card draws, so a card
   * that only recomputed when the pace ITSELF changed would freeze on the
   * zero it opened with - the frame counter is what has to bring the repaint,
   * and this walks a second of the take's own cadence to prove it does.
   */
  it('shows a pace that moved with the take', async () => {
    vi.useFakeTimers();
    try {
      const harness = open({ dictation: true });
      harness.page.record = bound('right_cmd', 'auto');
      flushSync();

      key('ControlRight', 'keydown');
      await opened();
      harness.page.record = bound('right_cmd', 'auto', take());
      flushSync();

      const card = () => document.querySelector('.tc')?.textContent ?? '';
      expect(card(), 'the pace it opened on, before a frame has moved').toContain('0 KB/s');

      // Fifty frames twenty milliseconds apart: 641 bytes each over the
      // window's own 0.98 s, which is 32_050 B/s.
      for (let at = 0; at < 50; at += 1) {
        vi.advanceTimersByTime(20);
        mic.held.onFrame?.(new Uint8Array(641));
      }
      flushSync();

      expect(card(), 'the pace a second of the take later').toContain('31 KB/s');
    } finally {
      vi.useRealTimers();
    }
  });

  /**
   * The graph reads this side's own frames, not the record's echo of them.
   *
   * The record's levels are whatever the core last metered and sent back; the
   * card's own reading is the audio it just produced. This drives frames the
   * socket does not take at all, which the record's levels never see - so a
   * card drawing those would hold still and this one moves.
   */
  it("reads the graph off its own frames, not the record's levels", async () => {
    const shared = wire();
    const harness = open({ dictation: true }, shared);
    harness.page.record = bound('right_cmd', 'auto');
    flushSync();

    key('ControlRight', 'keydown');
    await opened();
    harness.page.record = bound('right_cmd', 'auto', take());
    flushSync();

    // The take spans the draft's line, so the graph is the envelope: its
    // newest point is the last reading, and its y is what that reading drew.
    const newest = (): number | undefined => {
      const d = document.querySelector('.tc .wave path.area')?.getAttribute('d') ?? '';
      const match = / 100 ([\d.]+) L 100 100 L 0 100 Z$/.exec(d);
      return match === null ? undefined : Number(match[1]);
    };
    mic.held.onFrame?.(encodeFrame([0.9]));
    flushSync();
    expect(newest(), "a loud frame's own reading (94% tall, so y 6)").toBe(6);

    // A quieter frame the socket does not take: the record's levels never see
    // it, and the card's graph reads it anyway.
    shared.takes = false;
    mic.held.onFrame?.(encodeFrame([0.4]));
    flushSync();
    expect(newest(), 'and a quieter one, read where it was produced (83%, y 17)').toBe(17);
  });

  /**
   * The counts outlive the capture, because the take does.
   *
   * A local take lets go of the microphone at the release, while the record
   * keeps drawing the same take through transcription - so the counts the
   * page produced are kept until the card that draws them goes.
   */
  it('keeps the card counts while the take transcribes, after the release', async () => {
    vi.useFakeTimers();
    try {
      const harness = open({ dictation: true });
      harness.page.record = bound('right_cmd', 'auto');
      flushSync();

      key('ControlRight', 'keydown');
      await opened();
      harness.page.record = bound('right_cmd', 'auto', take());
      flushSync();

      for (let at = 0; at < 12; at += 1) mic.held.onFrame?.(new Uint8Array(641));
      vi.advanceTimersByTime(500);
      key('ControlRight', 'keyup');

      // The server moves the same take to transcribing, which is the card the
      // reader is looking at now; the page's own capture is already gone.
      harness.page.record = bound(
        'right_cmd',
        'auto',
        take({ phase: 'transcribing', progress: [2, 6] }),
      );
      flushSync();

      expect(drawn(), 'the card draws the take transcribing').toContain('2 of 6');
      expect(drawn(), 'with the frames it sent still beside it').toContain('12 fr');
      expect(drawn(), 'and no pace, which is a reading of a take still producing').not.toContain(
        'KB/s',
      );
    } finally {
      vi.useRealTimers();
    }
  });

  /**
   * The card's way out is Escape's way out.
   *
   * Both go through the take's own action, so the microphone is let go of
   * locally whichever the socket is doing, and the stop goes out when it can:
   * a close that only dispatched would do nothing at all on a closed socket.
   */
  it('abandons the take from the card, as Escape does', async () => {
    const harness = open({ dictation: true });
    harness.page.record = bound('right_cmd', 'auto');
    flushSync();

    key('ControlRight', 'keydown');
    await opened();
    harness.page.record = bound('right_cmd', 'auto', take());
    flushSync();

    const stops = mic.held.stops;
    const close = document.querySelector('.tc .x');
    if (!(close instanceof HTMLElement)) throw new Error('the card drew no way out');
    close.click();
    flushSync();

    expect(harness.sent.at(-1)?.command, 'the stop goes out as an abandon').toEqual({
      dictate_stop: { key: { org: 'Busytools', project: 'forge', label: 'lead' }, submit: false },
    });
    expect(mic.held.stops, 'and the microphone is let go of on this side too').toBe(stops + 1);
  });

  it('learns what the system default is from the first take that opens it', async () => {
    mic.held.resolved = { id: 'mic-2', label: 'Shure SM7B' };
    try {
      localStorage.removeItem('forge.dictate.default');
      const harness = open({ dictation: true });
      harness.page.record = bound('right_cmd', 'auto');
      flushSync();

      key('ControlRight', 'keydown');
      await opened();
      key('ControlRight', 'keyup');

      expect(
        localStorage.getItem('forge.dictate.default'),
        'the panel names the default from then on',
      ).toBe(JSON.stringify({ id: 'mic-2', label: 'Shure SM7B' }));
    } finally {
      mic.held.resolved = null;
      localStorage.removeItem('forge.dictate.default');
    }
  });

  it('learns nothing when the take opened a picked device', async () => {
    rememberDevice(SEAT, 'mic-9');
    mic.held.resolved = { id: 'mic-9', label: 'MacBook Pro Microphone' };
    try {
      const harness = open({ dictation: true });
      harness.page.record = bound('right_cmd', 'auto');
      flushSync();

      key('ControlRight', 'keydown');
      await opened();
      key('ControlRight', 'keyup');

      expect(
        localStorage.getItem('forge.dictate.default'),
        'a picked device is not the machine default',
      ).toBeNull();
    } finally {
      mic.held.resolved = null;
      rememberDevice(SEAT, null);
    }
  });

  it('learns nothing when the pick was cleared while the open ran', async () => {
    rememberDevice(SEAT, 'mic-2');
    mic.held.resolved = { id: 'mic-2', label: 'Shure SM7B' };
    mic.held.gated = true;
    try {
      const harness = open({ dictation: true });
      harness.page.record = bound('right_cmd', 'auto');
      flushSync();

      key('ControlRight', 'keydown'); // startTake captures the pick here
      await Promise.resolve();
      rememberDevice(SEAT, null); // the reader resets while the prompt is up
      mic.held.release?.(); // the permission lands; the picked stream resolves
      await opened();

      expect(
        localStorage.getItem('forge.dictate.default'),
        'the take asked for the picked device, so it names no default',
      ).toBeNull();
    } finally {
      mic.held.gated = false;
      mic.held.release = null;
      mic.held.resolved = null;
      rememberDevice(SEAT, null);
    }
  });

  /**
   * A second seat reaches the server, which owns the refusal.
   *
   * The take is one per connection, and a page's guard sees only its own:
   * a second seat's page holds take state of its own, so its press goes out
   * on the same connection - and the server, which alone knows what that
   * connection already holds, refuses it by name. A client that blocked
   * locally would be guessing about a take it cannot see.
   */
  it('lets a second seat reach the server, which owns the refusal', async () => {
    const shared = wire();
    app = mount(Harness, {
      target: document.body,
      props: {
        wire: shared,
        initial: { record: bound('right_cmd', 'auto'), dictation: true },
        dictation: true,
      },
    });
    flushSync();
    second = mount(Harness, {
      target: document.body,
      props: {
        wire: shared,
        initial: {
          record: record({
            slot: ELSEWHERE,
            composer: {
              take: null,
              notice: null,
              machine: null,
              bind: 'right_cmd',
              mode: 'auto',
            },
          }),
          slot: ELSEWHERE,
          dictation: true,
        },
        dictation: true,
      },
    });
    flushSync();

    // One press: the window listener is each page's own, so both pages hear
    // the same key and each asks for its own seat's take.
    key('ControlRight', 'keydown');
    await opened();
    key('ControlRight', 'keyup');

    const seats = shared.sent
      .flatMap((sent) => {
        const start = sent.command['dictate_stream'];
        return start === undefined ? [] : [(start as { key: SessionSlot }).key.label];
      })
      .sort();
    expect(seats, 'both seats pressed once, and both starts reach the one connection').toEqual([
      'lead',
      'other',
    ]);
  });

  /**
   * A gesture that lands while the microphone is still opening must END the
   * take it belongs to.
   *
   * The permission prompt is the everyday case for a first take in an
   * origin, and it is seconds long: a release dropped during it records past
   * the reader's hand until the next press stops it, which is the one gesture
   * this path had no cover for.
   */
  it('holds a release that lands while the microphone is opening', async () => {
    vi.useFakeTimers();
    mic.held.gated = true;
    mic.held.stops = 0;
    try {
      const harness = open({ dictation: true });
      harness.page.record = bound('right_cmd', 'hold');
      flushSync();

      key('ControlRight', 'keydown'); // the open begins and waits
      await Promise.resolve();
      key('ControlRight', 'keyup'); // the reader lets go during the prompt
      mic.held.release?.(); // the permission lands
      await opened();

      settle();
      expect(mic.held.stops, 'the take lets go of the microphone once its drain is done').toBe(1);
      expect(
        mic.held.onFrame,
        'and unhook the frames: audio posted after the release must go nowhere',
      ).toBeNull();
      expect(harness.sent.at(-1)?.command, 'the take is submitted, not left open').toEqual({
        dictate_stop: { key: { org: 'Busytools', project: 'forge', label: 'lead' }, submit: true },
      });
    } finally {
      mic.held.gated = false;
      mic.held.release = null;
      vi.useRealTimers();
    }
  });

  /**
   * Escape inside the open window is the TAKE's, not the page's: a reader
   * cannot see the difference between a take that is opening and one that is
   * live, so the key has to consume there too - and the take it cancels must
   * never record a sample.
   */
  it('consumes Escape inside the open window', async () => {
    mic.held.gated = true;
    mic.held.stops = 0;
    try {
      const harness = open({ dictation: true });
      harness.page.record = bound('right_cmd', 'hold');
      flushSync();

      key('ControlRight', 'keydown'); // the open begins and waits
      await Promise.resolve();
      window.dispatchEvent(
        new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }),
      );
      flushSync();
      mic.held.release?.(); // the permission lands
      await opened();

      expect(harness.sent.at(-1)?.command, 'the take is cancelled, not left open').toEqual({
        dictate_stop: { key: { org: 'Busytools', project: 'forge', label: 'lead' }, submit: false },
      });
      expect(mic.held.stops, 'and the microphone goes with it').toBe(1);
      expect(mic.held.onFrame, 'nothing records for a take the reader cancelled').toBeNull();
    } finally {
      mic.held.gated = false;
      mic.held.release = null;
    }
  });

  it('takes the binding off the record rather than assuming one', async () => {
    const harness = open({ dictation: true });
    harness.page.record = bound('left_cmd', 'auto');
    flushSync();

    key('ControlRight', 'keydown');
    await opened();
    expect(harness.sent, 'the key the config did not name is not the trigger').toEqual([]);

    key('ControlLeft', 'keydown');
    await opened();
    expect(harness.sent, 'the configured key is').toHaveLength(1);
  });

  it('arms nothing when the binding is off', () => {
    const harness = open({ dictation: true });
    harness.page.record = bound('off', 'auto');
    flushSync();

    key('ControlRight', 'keydown');
    key('MetaRight', 'keydown');
    expect(harness.sent, 'a key the config turned off starts no take').toEqual([]);
  });

  it('takes the mode off the record: a toggle stops on the press itself', () => {
    const harness = open({ dictation: true });
    harness.page.record = bound('right_cmd', 'toggle', take());
    flushSync();

    key('ControlRight', 'keydown');
    key('ControlRight', 'keyup');

    expect(harness.sent, 'the press IS the stop, and the release says nothing').toEqual([
      {
        command: {
          dictate_stop: {
            key: { org: 'Busytools', project: 'forge', label: 'lead' },
            submit: true,
          },
        },
      },
    ]);
  });

  it('leaves a chord alone: the take its press began is abandoned, not transcribed', async () => {
    const harness = open({ dictation: true });
    harness.page.record = bound('right_cmd', 'auto');
    flushSync();

    key('ControlRight', 'keydown');
    await opened();
    // Another key while the modifier is down is a chord, not a dictation.
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'c', code: 'KeyC', ctrlKey: true }));
    flushSync();
    key('ControlRight', 'keyup');

    expect(
      harness.sent.at(-1)?.command,
      'the speculative take goes, nothing is transcribed',
    ).toEqual({
      dictate_stop: { key: { org: 'Busytools', project: 'forge', label: 'lead' }, submit: false },
    });
  });

  it('abandons a live take on Escape, and leaves Escape alone otherwise', () => {
    const harness = open({ dictation: true });
    harness.page.record = bound('right_cmd', 'auto');
    flushSync();

    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    flushSync();
    expect(harness.sent, 'Esc takes a live take, and does nothing when there is none').toEqual([]);

    harness.page.record = bound('right_cmd', 'auto', take());
    flushSync();
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    flushSync();
    expect(harness.sent, 'a live take consumes Esc').toEqual([
      {
        command: {
          dictate_stop: {
            key: { org: 'Busytools', project: 'forge', label: 'lead' },
            submit: false,
          },
        },
      },
    ]);
  });

  it('starts nothing on an install that cannot dictate', () => {
    const harness = open();
    harness.page.record = bound('right_cmd', 'auto');
    flushSync();

    key('ControlRight', 'keydown');
    expect(harness.sent, 'the key follows the mic: no dictation, no take').toEqual([]);
  });

  /**
   * A stray modifier is not a chord. Any other bare modifier is consumed
   * without marking the hold chorded, because a modifier is not text and not a
   * shortcut - and the client marking one would DISCARD the take its press
   * began, losing the reader's words to a key they brushed.
   */
  it('leaves a stray modifier alone rather than chording the take', async () => {
    vi.useFakeTimers();
    try {
      const harness = open({ dictation: true });
      harness.page.record = bound('right_cmd', 'auto');
      flushSync();

      key('ControlRight', 'keydown');
      await opened();
      window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Shift', code: 'ShiftLeft' }));
      flushSync();
      vi.advanceTimersByTime(500);
      key('ControlRight', 'keyup');
      settle();

      expect(
        harness.sent.at(-1)?.command,
        'a brushed modifier must not lose what the reader said',
      ).toEqual({
        dictate_stop: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          submit: true,
        },
      });
    } finally {
      vi.useRealTimers();
    }
  });

  /**
   * A live take consumes Escape, the same rule the view this client replaced
   * applied and why the client's surfaces cannot both have it: the list under
   * the field closes on an Escape nobody else took, and the dock's own Escape
   * is the take's.
   */
  it('gives Escape to a live take rather than to the list underneath it', () => {
    const harness = open({ dictation: true });
    // The state is reachable one way now: the reader types the `/` that opens
    // the list, and the take arrives while it stands. (A take owns the draft's
    // line, so it cannot be typed into once one runs.)
    type('/');
    expect(document.querySelector('.ac'), 'a list is open under the field').not.toBeNull();

    harness.page.record = bound('right_cmd', 'auto', take());
    flushSync();

    press('Escape');

    expect(harness.sent, 'the take takes the key').toEqual([
      {
        command: {
          dictate_stop: {
            key: { org: 'Busytools', project: 'forge', label: 'lead' },
            submit: false,
          },
        },
      },
    ]);
    expect(document.querySelector('.ac'), 'and the list stands').not.toBeNull();
  });

  /**
   * A held key repeats, and the view this client replaced treated every repeat
   * as carrying no instruction. Without the same here a toggle take stops once
   * per repeat: one stopping press and two repeats gave three stops for one
   * keypress.
   */
  it('ignores a key repeat rather than stopping once per repeat', () => {
    const harness = open({ dictation: true });
    harness.page.record = bound('right_cmd', 'toggle', take());
    flushSync();

    key('ControlRight', 'keydown');
    key('ControlRight', 'keydown', true);
    key('ControlRight', 'keydown', true);

    expect(harness.sent, 'one press asks for one stop, however long the key is held').toHaveLength(
      1,
    );
  });

  it('abandons once when the dock holds both the take and the keyboard', () => {
    const harness = open({
      dictation: true,
      record: record({
        pending_asks: [permissionAsk()],
        composer: { take: take(), notice: null, machine: null },
      }),
    });
    flushSync();
    expect(document.querySelector('.dock'), 'the dock has the box').not.toBeNull();

    press('Escape');

    expect(harness.sent, 'one keypress is one command').toEqual([
      {
        command: {
          dictate_stop: {
            key: { org: 'Busytools', project: 'forge', label: 'lead' },
            submit: false,
          },
        },
      },
    ]);
  });
});

/**
 * The seat's landed words are not every surface's to take.
 *
 * A take begun on another page of the same seat - a review box, a document's
 * hand-off box, the files page - lands on the seat, and the record re-hands
 * it with `carried` for the page that reloaded mid-landing. The composer took
 * any carried landing, so returning to the session filled its draft with
 * words the reader had already put in a review box.
 */
describe('a landing the seat carries', () => {
  it('does not take a carried landing another surface began', () => {
    open({
      record: withNotice({ ...LANDED, carried: true, surface: 'review' }),
      dictation: true,
    });
    expect(field().value, 'the review box already took these words').toBe('');
  });

  it('does not take a live landing another surface began', async () => {
    const harness = open({ dictation: true });
    key('ControlRight', 'keydown');
    await opened();
    harness.page.record = withNotice({ ...LANDED, surface: 'review' });
    flushSync();
    // The take has to end for the box to draw its field again. Ending it with
    // no words leaves the foreign landing as the only thing that could have
    // reached this draft.
    harness.say({
      kind: 'update',
      update: { dictate_ended: { key: SLOT, outcome: 'cancelled', generation: 1 } },
    });
    flushSync();
    expect(field().value, 'this box began no review take, and keeps its words out').toBe('');
  });

  /**
   * **The page that comes back to the session** (or reloads) reads the record
   * once, and the seat's carried landing is its own take's words: the composer
   * still places them - and the record's next hand-over of the same notice
   * appends nothing.
   */
  it('takes its own carried landing after the page comes back, once', () => {
    const carried = { ...LANDED, carried: true, surface: 'composer' };
    const harness = open({ record: withNotice(carried), dictation: true });
    expect(field().value, "a re-handed composer take is still the composer's to place").toBe(
      LANDED.text,
    );
    harness.page.record = withNotice(carried);
    flushSync();
    expect(field().value, 'the second hand-over of the same notice appends nothing').toBe(
      LANDED.text,
    );
  });

  /**
   * **A server older than the field degrades to the old behaviour**: the
   * record names no surface, so nothing refuses the words and the landing
   * places as it always did - never a silent drop.
   */
  it('places a carried landing a record without a surface re-hands', () => {
    open({ record: withNotice({ ...LANDED, carried: true }), dictation: true });
    expect(field().value, 'the old server has no surface to refuse').toBe(LANDED.text);
  });
});

/**
 * #1411: a message the reader has sent must not come back into the box.
 *
 * The draft is cleared on send, so the words arriving again are the second
 * reading rather than the first: something wire-driven writes them back. The
 * only wire-driven write is a take landing, and the guard on it is what these
 * cases are about.
 */
describe('a sent message', () => {
  /**
   * The composer's own contract, and the one this file can speak for: a
   * landing that is still the record's notice lands ONCE, however many times
   * the record is handed over. **The spent arm is what holds it** - the words
   * are placed by the take's own outcome, and there is no arm left to place
   * them again on a re-hand-over.
   *
   * **What this is not evidence for.** #1411's defect is a whole-record read
   * published while it was older than the frames applied since, which
   * re-delivers a landing the box has already taken - and the guard for that
   * is the page's, in `session/live.ts`, because the composer cannot tell a
   * re-delivered landing from a new take's: the words and the notice are the
   * same. So a page that hands this component the stale record still gets the
   * words landed twice, and this test passing says nothing about the page's.
   * The two are separate rules and neither stands for the other.
   */
  it('lands the words once while the notice is held', async () => {
    const harness = open({ dictation: true });
    await beginTake(harness);
    harness.page.record = withNotice(LANDED);
    flushSync();
    expect(field().value, 'the take lands in the box').toBe(LANDED.text);

    harness.page.record = withNotice(LANDED);
    flushSync();

    expect(field().value, 'the words land once, not once per record handed over').toBe(LANDED.text);
  });

  /**
   * **The arm survives the landing it was set under, re-handed.** The press for
   * the next take happens while the previous landing still sits on the record
   * (the notice only goes null when the server's take-start arrives), and any
   * frame in that window hands that same notice over again as a fresh object:
   * consuming the arm on it left the new take's own words nowhere to go (Ved,
   * live, 2026-10-15).
   */
  it('places a take pressed under a landing the record re-hands', async () => {
    const harness = open({ dictation: true });
    await beginTake(harness);
    harness.page.record = withNotice(LANDED);
    flushSync();
    sendBox();

    // The next take is pressed with that landing still on the record, and the
    // record hands it over AGAIN before the take's own start.
    key('ControlRight', 'keydown');
    await opened();
    key('ControlRight', 'keyup');
    harness.page.record = withNotice(LANDED);
    flushSync();

    // The take's start, then its own landing.
    harness.page.record = withNotice(null);
    flushSync();
    endTake(harness);
    harness.page.record = withNotice({ ...LANDED, text: 'and run the gate too' });
    flushSync();

    expect(field().value, 'the take set under a landing still places its own words').toBe(
      'and run the gate too',
    );
  });

  /**
   * **The same words spoken twice are two landings.** The record hands a
   * notice over on every write, so the take's own number is what tells the
   * first landing re-read from a second take that spoke the same words - and
   * this second take is quick, its start folded into one write, so no record
   * ever clears the first notice before its own lands (the gate's F2,
   * 2026-10-15).
   */
  it('places the same words a second time when another take spoke them', async () => {
    const harness = open({ dictation: true });
    await beginTake(harness);
    harness.page.record = withNotice({ ...LANDED, generation: 1 });
    flushSync();
    sendBox();

    // The second take is quick: the press happens under the first landing's
    // notice, and its own words land without any record clearing it.
    key('ControlRight', 'keydown');
    await opened();
    key('ControlRight', 'keyup');
    endTake(harness);
    harness.page.record = withNotice({ ...LANDED, generation: 2 });
    flushSync();

    expect(field().value, 'the second take lands words of its own').toBe(LANDED.text);
  });

  it('still takes a landing whose words are new', async () => {
    const harness = open({ dictation: true });
    await beginTake(harness);
    harness.page.record = withNotice(LANDED);
    flushSync();
    sendBox();

    // The next take begins here too, with the previous landing still on the
    // record at the press: an arm is spent by the landing it placed, so the
    // words of a take this page never began are not its own, and the notice
    // already showing must not spend the arm either (Ved, live, 2026-10-15).
    await beginTake(harness);
    harness.page.record = withNotice({ ...LANDED, text: 'and run the gate too' });
    flushSync();

    expect(field().value, "the next take's words land as they always did").toBe(
      'and run the gate too',
    );
  });

  /**
   * #1499, the same property one move over: words the reader already sent must
   * not come back into the box.
   *
   * The page hands this component one record after another as the reader moves
   * between seats, and the composer itself stays mounted across that move. The
   * landed notice is the SEAT's, and the core holds it until that seat starts
   * another take - so a box that reads the notice it meets on returning as a
   * landing it has not taken puts back words that were sent before the reader
   * left.
   */
  it("does not put a sent take's words back when the reader leaves the seat and returns", async () => {
    const harness = open({ dictation: true });
    await beginTake(harness);
    harness.page.record = withNotice(LANDED);
    flushSync();
    // The premise, without which a landing that settles nothing leaves this
    // case green: the take is in the box, and the words that come back are the
    // ones the reader sent out of it.
    expect(field().value, 'the take lands in the box').toBe(LANDED.text);
    sendBox();
    expect(field().value, 'the reader sent the words, so the box is empty').toBe('');

    // Another seat, which has no take of its own.
    harness.page.slot = ELSEWHERE;
    harness.page.record = withNotice(null);
    flushSync();

    // Back to the seat the take landed on, whose notice the core still holds.
    harness.page.slot = SLOT;
    harness.page.record = withNotice(LANDED);
    flushSync();

    expect(field().value, 'the words the reader already sent came back into the box').toBe('');
  });
});

/**
 * The box belongs to the seat, not to this component.
 *
 * The page hands the composer one record after another as the reader moves
 * between seats, and the composer stays mounted across the move: so everything
 * the reader's own doing leaves behind - their words, the landing they have
 * taken, what a send is still waiting on - is kept per seat and swapped when
 * the seat is. A view holds the same state the same way, on its own session
 * record rather than on the app, which is what makes a draft come back with
 * its seat and stops one seat's words reaching another's box.
 */
describe('the seat the box belongs to', () => {
  it('keeps the draft for its own seat, and shows none of it on another', () => {
    const harness = open();
    type('fix the flaky retry');

    harness.page.slot = ELSEWHERE;
    flushSync();
    expect(field().value, "another seat's box is not this reader's draft").toBe('');

    harness.page.slot = SLOT;
    flushSync();
    expect(field().value, 'the draft comes back with the seat it was typed on').toBe(
      'fix the flaky retry',
    );
  });

  it('lands a take only on the seat that watched it, and lands it there', async () => {
    const harness = open({ dictation: true });
    // The seat's own take, which is what arms a landing for THIS box.
    await beginTake(harness);

    // A landing on another seat is not this box's to take: this box never
    // watched that seat's take, so the words would be put back where nothing of
    // the reader's was ever typed.
    harness.page.slot = ELSEWHERE;
    harness.page.record = withNotice(LANDED);
    flushSync();
    expect(field().value, 'a take this box never watched is not its to land').toBe('');

    // And back on the seat whose take it did watch, a landing lands HERE - the
    // seat the record says it belongs to, not the one just left.
    harness.page.slot = SLOT;
    harness.page.record = withNotice({ ...LANDED, text: 'and run the gate too' });
    flushSync();
    expect(field().value, 'the words land in the box of the seat they belong to').toBe(
      'and run the gate too',
    );
  });

  /**
   * The move itself is not enough when the destination box has taken a take
   * of its own before, which is every seat the reader dictates on: what stops
   * this landing is which seat the record in hand belongs to - and right after
   * a move that record is still the seat being LEFT's, until the one being
   * moved to answers.
   */
  it('does not land the left seat take in the box of the seat it moved to', async () => {
    const harness = open({ dictation: true });
    // Both seats are the reader's, so both boxes have watched a take: a fresh
    // box would refuse the landing below for the wrong reason.
    await beginTake(harness);
    harness.page.slot = ELSEWHERE;
    harness.page.record = withNotice(null, null, ELSEWHERE);
    flushSync();
    await beginTake(harness, ELSEWHERE);

    // On the first seat a take lands and the reader sends it. The core keeps
    // the landed notice after the take is gone.
    harness.page.slot = SLOT;
    harness.page.record = withNotice(LANDED);
    flushSync();
    expect(field().value, 'the take lands on its own seat').toBe(LANDED.text);
    sendBox();
    expect(field().value, 'the reader sent the words, so the box is empty').toBe('');

    // The reader moves, and the page has not answered with the new seat yet:
    // what it still holds is the seat they left, landing and all.
    harness.page.slot = ELSEWHERE;
    flushSync();

    expect(
      field().value,
      "the seat they left's landing was put into the box of the seat they moved to",
    ).toBe('');
  });

  /**
   * **The arm lives on the box the take began on, not the one on screen.** A
   * press on one seat, a move to another while the microphone opens, and the
   * cancel that follows: clearing the arm on the seat being SHOWN leaves the
   * take's own box armed, and a landing there is then placed by a take that
   * was cancelled (the gate's Important, 2026-10-15).
   */
  it('drops the arm on the box the take began on, cancelled from another seat', async () => {
    const harness = open({ dictation: true });

    key('ControlRight', 'keydown');
    await opened();
    key('ControlRight', 'keyup');

    // The reader moves on while the take they began is still this page's, and
    // abandons it there.
    harness.page.slot = ELSEWHERE;
    harness.page.record = withNotice(null, take(), ELSEWHERE);
    flushSync();
    press('Escape');

    // Back on the seat the take began on, where another client's take lands.
    harness.page.slot = SLOT;
    harness.page.record = withNotice(LANDED);
    flushSync();

    expect(field().value, "the cancelled take's arm rode a landing that was not its own").toBe('');
  });

  /**
   * **The record's own copy of the armed take's landing reaches the box.**
   * A whole-record read taken across the landing frame carries the words as
   * `carried`, and the take's generation - which the page holds while its
   * take is drawn - is what tells them from an older re-hand and places them,
   * rather than leaving them for a frame that never comes (#2283).
   */
  it("places the armed take's own carried landing from the record", async () => {
    const harness = open({ dictation: true });

    key('ControlRight', 'keydown');
    await opened();
    key('ControlRight', 'keyup');

    // The read lands holding the take the microphone is still recording,
    // with that take's own landing re-handed beside it. A live take draws its
    // card rather than the field, so the take has to end before the box it
    // filled is on screen.
    const carried = {
      kind: 'landed',
      text: 'the words',
      truncated: false,
      generation: 4,
      carried: true,
    };
    harness.page.record = withNotice(carried, take({ generation: 4 }));
    flushSync();
    endTake(harness);
    harness.page.record = withNotice(carried);
    flushSync();

    expect(field().value, "the take's own words land in the box").toBe('the words');
  });

  it('does not mark a refused send on a seat that never sent it', () => {
    const shared = wire();
    const harness = open({}, shared);
    type('push it once CI is green');
    sendBox();
    // The premise, without which a refusal that never reaches any seat leaves
    // this case green: this seat's own send really is outstanding, so a
    // refusal names it.
    shared.say({ kind: 'error', what: 'dispatch', why: 'the seat is busy' });
    flushSync();
    expect(echoAt(SLOT), 'the seat waiting on it is the one a refusal names').toMatchObject({
      state: 'failed',
      why: 'the seat is busy',
    });

    // Sent again, and the reader moves on before the core answers. The send
    // stays with the seat it belongs to rather than going with the reader: a
    // refusal that arrives now still has to reach the row that is waiting on
    // it, and a page that dropped the send here would lose it in silence.
    type('and the gate too');
    sendBox();
    expect(harness.sent, 'the second send went out too').toHaveLength(2);
    harness.page.slot = ELSEWHERE;
    flushSync();
    expect(
      echoAt(SLOT),
      'leaving a seat does not give up the send it was waiting on',
    ).toMatchObject({ state: 'sending', words: 'and the gate too' });

    // The core refuses that one, and the connection says so to every page on it
    // - the error names the operation, never the seat.
    shared.say({ kind: 'error', what: 'dispatch', why: 'that seat is gone' });
    flushSync();

    expect(echoAt(SLOT), 'the refusal lands on the seat that sent it').toMatchObject({
      state: 'failed',
      why: 'that seat is gone',
    });
    expect(
      echoAt(ELSEWHERE),
      'a seat that sent nothing is not where a refusal lands',
    ).toBeUndefined();
    expect(field().value, 'and its box is not where it lands either').toBe('');
  });

  /**
   * **A refusal names the press it refuses.** The server echoes the dispatch
   * id the send minted, so two seats waiting on a send are not both marked by
   * one refusal - and a refusal that arrives after the reader moved still
   * finds the seat it belongs to (#1583).
   */
  it('routes a refusal to the dispatch it names, not to every waiting seat', () => {
    const shared = wire();
    const harness = open({}, shared);
    type('push it once CI is green');
    sendBox();
    const first = harness.sent[0]?.command['prompt_under'] as Record<string, unknown>;

    // The reader moves on with that send still outstanding, and sends again
    // from the seat they moved to.
    harness.page.slot = ELSEWHERE;
    flushSync();
    type('and the gate too');
    sendBox();
    expect(harness.sent, 'both sends went out').toHaveLength(2);
    const second = harness.sent[1]?.command['prompt_under'] as Record<string, unknown>;
    expect(first['dispatch_id'], 'each press minted its own id').not.toBe(second['dispatch_id']);

    shared.say({
      kind: 'error',
      what: 'dispatch',
      why: 'that seat is gone',
      dispatch_id: first['dispatch_id'] as number,
    });
    flushSync();

    expect(echoAt(SLOT), 'the seat the refusal names is the one marked').toMatchObject({
      state: 'failed',
      why: 'that seat is gone',
    });
    expect(
      echoAt(ELSEWHERE),
      'and the seat still waiting on its own send is untouched',
    ).toMatchObject({ state: 'sending', words: 'and the gate too' });
  });

  /**
   * **A numbered refusal whose press is gone marks nothing.** A second quick
   * send takes the seat's one echo, so the older press may hold none - and
   * that older reason must not be read against the live send (#1583).
   */
  it('drops a numbered refusal whose own press the seat has replaced', () => {
    const shared = wire();
    const harness = open({}, shared);
    type('the first words');
    sendBox();
    const first = harness.sent[0]?.command['prompt_under'] as Record<string, unknown>;

    type('the second words');
    sendBox();
    expect(harness.sent, 'both sends went out').toHaveLength(2);
    const second = harness.sent[1]?.command['prompt_under'] as Record<string, unknown>;
    expect(first['dispatch_id']).not.toBe(second['dispatch_id']);

    shared.say({
      kind: 'error',
      what: 'dispatch',
      why: 'that seat is gone',
      dispatch_id: first['dispatch_id'] as number,
    });
    flushSync();

    expect(
      echoAt(SLOT),
      'the send on screen is not marked with the press it replaced',
    ).toMatchObject({ state: 'sending', words: 'the second words' });
  });
});

// A path rather than a URL: this file runs under jsdom, where `import.meta.url`
// is the dev server's and not a file the disk can be read at. It resolves
// against the client directory, which is where every recipe runs this from.
const sheet = readFileSync('src/assets/web.css', 'utf8');

/** The last rule body the sheet writes for this exact selector. */
function sheetRule(selector: string): string {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const found = [...sheet.matchAll(new RegExp(`^\\s*${escaped}\\s*\\{([^}]*)\\}`, 'gm'))];
  const body = found.at(-1)?.[1];
  // A miss must not read as an empty rule: a not.toContain leg would pass on it.
  if (body === undefined) throw new Error(`the sheet writes no rule for ${selector}`);
  return body;
}

/**
 * The FIRST rule body the sheet writes for this exact selector: the base rule,
 * which a media block may override later.
 *
 * **The two helpers are not interchangeable, and reaching for the wrong one
 * makes a test that cannot fail.** `.dict` is re-written inside the narrow
 * media block for its wrap, so resolving the LAST match reads that rule and an
 * inset put back on the base one passes the suite while the browser paints it.
 */
function baseRule(selector: string): string {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const found = [...sheet.matchAll(new RegExp(`^\\s*${escaped}\\s*\\{([^}]*)\\}`, 'gm'))];
  const body = found[0]?.[1];
  if (body === undefined) throw new Error(`the sheet writes no rule for ${selector}`);
  return body;
}

/**
 * The ring the box draws, resolved the way a browser resolves it.
 *
 * The ring's colour is a precedence question rather than a text one: a rule
 * that relies on where it sits in the sheet reads exactly like one that states
 * what it means, and the two part company only when a state arrives in the
 * wrong place. So every rule the sheet writes for the box ITSELF is matched
 * against the classes the component set and whether the keyboard is inside it,
 * the most specific wins, and source order settles a tie.
 */

/** What a box rule asks of the element it paints. */
interface Need {
  /** Classes the box must carry. */
  classes: string[];
  /** Whether the keyboard must be inside it. */
  focus: boolean;
  /** Classes it must not carry, one per `:not(...)` argument. */
  without: string[];
}

/** A rule the sheet writes that names the box itself. */
interface BoxRule {
  /** The part of the selector that names the box, as the sheet writes it. */
  selector: string;
  /** The rule's own text, which is where a second device on the box would be. */
  body: string;
  need: Need;
  strength: number;
}

/** A rule that paints the box's ring, which is what a state is read from. */
interface Ring extends BoxRule {
  colour: string;
}

/**
 * What a selector asks of a box.
 *
 * `null` is only for a rule that selects something INSIDE the box, which is
 * not the box's own ring. Anything else the resolver cannot read throws: a
 * rule it skipped is invisible to every check here, so the suite would read
 * green about a state it never saw.
 */
function need(selector: string): Need | null {
  const rest = selector.replace(/^\.surf/, '');
  if (/[\s>+~]/.test(rest.replace(/\([^)]*\)/g, ''))) return null;
  const found: Need = { classes: [], focus: false, without: [] };
  let at = 0;
  while (at < rest.length) {
    const tail = rest.slice(at);
    const classy = /^\.([a-z0-9-]+)/.exec(tail);
    const focused = /^:focus-within/.exec(tail);
    const excluded = /^:not\(([^)]*)\)/.exec(tail);
    if (classy?.[1] !== undefined) {
      found.classes.push(classy[1]);
      at += classy[0].length;
    } else if (focused !== null) {
      found.focus = true;
      at += focused[0].length;
    } else if (excluded?.[1] !== undefined) {
      found.without.push(...excluded[1].split(',').map((name) => name.trim().replace(/^\./, '')));
      at += excluded[0].length;
    } else {
      throw new Error(`a rule for the box in a form this resolver cannot read: ${selector}`);
    }
  }
  return found;
}

/**
 * The border colour a rule paints, written longhand or in the shorthand, or
 * `null` when it paints no border at all - which is a rule about something
 * other than the ring. A shorthand it cannot read throws, for the reason
 * `need` does.
 */
function painted(body: string): string | null {
  const longhand = /border-color:\s*([^;]+)/.exec(body);
  if (longhand?.[1] !== undefined) return longhand[1].trim();
  const shorthand = /border:\s*([^;]+)/.exec(body);
  if (shorthand?.[1] === undefined) return null;
  // Width, style, colour: the colour is the third token. Fewer than three
  // names none at all, which is a reset and not a ring; more than three is not
  // a border shorthand, so it is a form this resolver cannot read.
  const parts = shorthand[1].trim().split(/\s+/);
  if (parts.length < 3) return null;
  if (parts.length > 3) {
    throw new Error(`a border on the box in a form this resolver cannot read: ${shorthand[0]}`);
  }
  return parts[2] ?? null;
}

/** A selector naming the frame itself: the class is bounded after its letters. */
const BOX = /^\.surf(?![a-z0-9-])/;

/** The parts of a selector list, split outside any parentheses - a `:not(...)` carries its own commas. */
function listed(selector: string): string[] {
  const parts: string[] = [];
  let depth = 0;
  let at = 0;
  for (let i = 0; i < selector.length; i += 1) {
    const ch = selector[i];
    if (ch === '(') depth += 1;
    else if (ch === ')') depth -= 1;
    else if (ch === ',' && depth === 0) {
      parts.push(selector.slice(at, i));
      at = i + 1;
    }
  }
  parts.push(selector.slice(at));
  return parts.map((part) => part.trim()).filter((part) => part !== '');
}

/**
 * Every rule the sheet writes that names the box itself, in source order.
 *
 * The set is every flat rule rather than every line that starts with the
 * class, because the sheet groups selectors and a rule naming the box paints
 * the box wherever in the list it sits. A part selecting something inside the
 * box is not the box's own rule; a selector the resolver cannot read throws,
 * for the reason `need` gives.
 */
function boxRules(text: string): BoxRule[] {
  const found: BoxRule[] = [];
  // Comments go first: a comment sitting above a rule is otherwise read as part
  // of that rule's selector, and a comment inside a body as part of the body -
  // where a rule that is only talked about would read as a rule that is drawn.
  const sheet = text.replace(/\/\*[\s\S]*?\*\//g, '');
  const rules = [...sheet.matchAll(/^[ \t]*([^@{}][^{}]*?)\s*\{([^{}]*)\}/gm)];
  for (const rule of rules) {
    const body = rule[2] ?? '';
    for (const selector of listed(rule[1] ?? '')) {
      if (!BOX.test(selector)) continue;
      const ask = need(selector);
      if (ask === null) continue;
      // A class and a pseudo-class are one of specificity each, and a
      // `:not(...)` is its most specific argument rather than all of them.
      const strength = ask.classes.length + (ask.focus ? 1 : 0) + (ask.without.length > 0 ? 1 : 0);
      found.push({ selector, body, need: ask, strength });
    }
  }
  return found;
}

/** The rules that paint the box's ring, which is what a state is read from. */
function rings(text: string): Ring[] {
  const found: Ring[] = [];
  for (const rule of boxRules(text)) {
    const colour = painted(rule.body);
    if (colour === null) continue;
    found.push({ ...rule, colour });
  }
  return found;
}

/** Every rule naming the box that draws the inset line, which is the device this change deletes. */
function insetRules(text: string): string[] {
  return boxRules(text)
    .filter((rule) => /box-shadow:[^;]*inset/.test(rule.body))
    .map((rule) => rule.selector);
}

/** The colour the sheet gives a box carrying `classes`, keyboard in or out. */
function ring(text: string, classes: string[], focus: boolean): string {
  const contenders = rings(text).filter((rule) => {
    if (rule.need.focus && !focus) return false;
    if (rule.need.classes.some((name) => !classes.includes(name))) return false;
    return rule.need.without.every((name) => !classes.includes(name));
  });
  const winner = contenders.reduce<Ring | null>(
    (best, rule) => (best === null || rule.strength >= best.strength ? rule : best),
    null,
  );
  if (winner === null) throw new Error(`the sheet paints no ring for .box.${classes.join('.')}`);
  return winner.colour;
}

/**
 * The frame is one ring with one meaning: the composer focuses its field on
 * mount, so the box a reader types into carries the accent, and what the box is
 * DOING takes the ring from focus. An ordering that happens to pick the right
 * colour reads the same as a stated precedence, so the colour is resolved here
 * the way a browser resolves it.
 */
describe('the frame', () => {
  /** Every state the box draws, and the colour the ring takes for it. */
  const STATES: readonly [state: string, colour: string][] = [
    ['rec', 'color-mix(in srgb, var(--accent) 35%, var(--hot))'],
    ['tr', 'var(--blue)'],
    ['done', 'var(--ok)'],
    ['err', 'var(--bad)'],
  ];

  it('answers the keyboard at rest: the accent when it is here, the control border when it is not', () => {
    expect(ring(sheet, [], false), 'a box at rest with the keyboard elsewhere').toBe('var(--ctl)');
    expect(ring(sheet, [], true), 'a box at rest with the keyboard here').toBe('var(--accent)');
  });

  for (const [state, colour] of STATES) {
    it(`draws ${state} in its own colour, whatever the keyboard is doing`, () => {
      expect(ring(sheet, [state], false), `${state}, keyboard elsewhere`).toBe(colour);
      expect(ring(sheet, [state], true), `${state}, keyboard here`).toBe(colour);
    });
  }

  /**
   * The precedence, resolved over the sheet rather than pattern-matched: the
   * states come from the sheet itself, so one arriving in the wrong place is
   * covered the moment it exists.
   */
  it('lets every state the sheet draws take the ring from focus', () => {
    const states = new Set(rings(sheet).flatMap((rule) => rule.need.classes));
    expect(states.size, 'the sheet draws no state of its own').toBeGreaterThan(0);
    for (const state of states) {
      expect(ring(sheet, [state], true), `focus repaints the ring while the box is ${state}`).toBe(
        ring(sheet, [state], false),
      );
    }
  });

  /**
   * The control for the check above: on a box doing nothing, focus DOES change
   * the ring, so a resolver that could see no difference at all would be
   * caught rather than read as precedence holding.
   */
  it('can tell focus apart where the box is doing nothing', () => {
    expect(ring(sheet, [], true), 'the check sees no difference focus makes').not.toBe(
      ring(sheet, [], false),
    );
  });

  /**
   * The negative control, and the failure the precedence check exists for: a
   * state added ahead of the focus rule, where a cascade relying on source
   * order silently repaints it. This is the sheet with one such state spliced
   * in, and the check has to report it.
   */
  it('reports a state that arrived in front of the focus rule', () => {
    const mutated = sheet.replace(
      '.surf:focus-within',
      '.surf.warn { border-color: var(--warn); }\n.surf:focus-within',
    );
    expect(
      ring(mutated, ['warn'], true),
      'a state the focus rule silently overpaints is not reported',
    ).not.toBe(ring(mutated, ['warn'], false));
  });

  /**
   * The denominator under the two checks above: a rule for the box that this
   * resolver cannot read must fail rather than be skipped, because a skipped
   * rule is invisible to both of them and the suite reads green about a state
   * it never saw.
   */
  it('fails on a rule for the box it cannot read, rather than skipping it', () => {
    const spliced = sheet.replace(
      '.surf:focus-within',
      '.surf[data-phase="warn"] { border-color: var(--warn); }\n.surf:focus-within',
    );
    expect(
      () => ring(spliced, ['warn'], false),
      'a rule for the box this resolver cannot read is skipped',
    ).toThrow(/cannot read/);
  });

  /**
   * The other failing direction, and the one that is valid CSS: a border that
   * names no colour is a reset rather than a ring, so it paints nothing for the
   * resolver to read and must not fail for saying so.
   */
  it('skips a border that names no colour rather than reading it as the ring', () => {
    const spliced = sheet.replace('.surf.rec', '.surf.reset { border: 0; }\n.surf.rec');
    expect(ring(spliced, ['reset'], false), 'a reset is read as a ring colour').toBe('var(--ctl)');
  });

  it('draws no second device: no rule for the box paints an inset line', () => {
    expect(insetRules(sheet), 'a rule for the box draws the inset line again').toEqual([]);
  });

  /**
   * One box, one left margin. The field, the notice and the blocking states
   * all start where the field starts; the take's card is the one thing that
   * does not, because it rides the box's right corner rather than the column.
   *
   * Measured in a browser before this: the notice's and the (then) dictation
   * row's TEXT sat 25.00px right of the field's, at 1600 and at 430 - a gutter
   * inherited from the view this client replaced rather than chosen, and 13px
   * more than the issue that found it believed. A sheet assertion cannot see
   * that offset; what it can do is keep the inset from coming back, which is
   * the change someone would plausibly make.
   *
   * **What it reads: the base rules only.** A `padding-left` added inside a
   * media block passes this, because the guard resolves the first rule each
   * selector has. The shipped sheet is right either way; the limit is stated
   * so a reader does not take this for wider than it is.
   */
  it('starts every row at the left edge of the field', () => {
    for (const row of ['.comp .notice', '.blocked', '.blocked .b2']) {
      expect(baseRule(row), `${row} carries a left inset the field does not`).not.toMatch(
        /padding-left:\s*[1-9]/,
      );
    }
  });

  /**
   * The ladder's steps are container queries, so the box has to BE the
   * container: without this the queries never match and the card simply grows
   * past its box at every width below the full reading.
   */
  it('makes the surface the container the card ladder measures', () => {
    expect(baseRule('.surf'), 'the surface is not a container, so no step can fire').toContain(
      'container-type: inline-size',
    );
  });

  it('caps a long option list rather than growing the lane past the viewport', () => {
    // Review Focus 4: eight options on a phone must scroll in place, with the
    // words row below them still reachable. **Deliberately not pointer-scoped**
    // (the plan said "under a coarse pointer"): a lane should not grow past the
    // viewport on any input, and 46vh/420px is a generous desktop measure too.
    const rule = baseRule('.dock .opts');
    expect(rule, 'the option list has no cap').toContain('max-height');
    // The 46vh component is what holds the decision above: a cap weakened to
    // a fixed 4000px would still read as "a cap" without this.
    expect(rule, 'the cap no longer measures against the viewport').toContain('46vh');
    expect(rule, 'and does not scroll').toContain('overflow-y: auto');
  });

  it('resets the UA button chrome off the folded bar', () => {
    // The bar is a real `<button>`, so without the house reset it wears the
    // UA sheet - measured against the real sheet: an rgb(239,239,239) ground,
    // a 2px outset border around it, Arial - light-mode chrome inside the
    // dark surface, visible whenever a prompt is folded.
    const rule = baseRule('.dock .qbar');
    expect(rule, 'the bar keeps the UA ground').toContain('background: none');
    expect(rule, 'and the UA border').toContain('border: 0');
    expect(rule, 'and the UA font').toContain('font: inherit');
  });

  /**
   * The card's ladder: as the box narrows, one reading leaves at a time from
   * the left, and the dot and the way out are never among them.
   *
   * Read off the sheet by name, because jsdom performs no layout: what it can
   * hold is the ORDER and the never-removed pair, which is the shape a later
   * edit would plausibly break by hiding something else at a smaller width.
   */
  it('removes the card readings one at a time from the left, and never the dot', () => {
    const steps = [
      ...sheet.matchAll(/@container \(max-width: (\d+)px\) \{ ([^{]+) \{ display: none; \} \}/g),
    ]
      .map((match) => ({ width: Number(match[1]), hidden: (match[2] ?? '').trim() }))
      // The sheet holds container queries for other surfaces too; this asks
      // only about the card's own steps.
      .filter((step) => step.hidden.startsWith('.tc'));
    // The section bar and the count are the transcribing anatomy of the same
    // two slots, so they leave with the graph and the tally they replace.
    const leaving = [
      '.tc .clock',
      '.tc .bars, .tc .wave, .tc .ticks, .tc .bar',
      '.tc .ready, .tc .count',
      '.tc .fr',
      '.tc .kb',
      '.tc .pace',
    ];
    expect(
      steps.map((step) => step.hidden),
      'the ladder hides something other than the readings, or reorders them',
    ).toEqual(leaving);
    for (let at = 1; at < steps.length; at += 1) {
      expect(
        steps[at]?.width,
        'a step is wider than the one before it, so the order would flip',
      ).toBeLessThan(steps[at - 1]?.width ?? 0);
    }
    for (const kept of ['.tc .dot', '.tc .spin', '.tc .x']) {
      expect(sheet, `the ladder removes ${kept}, which no width may`).not.toMatch(
        new RegExp(`@container[^}]*${kept.replace('.', '\\.')}`),
      );
    }
  });

  /**
   * The composer's notice row shares its class with the chat's delivery
   * notices, so its rules are scoped to the composer: a bare rule sits later in
   * the sheet than the chat's and wins for every delivery on the page, which
   * measured as a delivery losing its padding, margin, radius and border.
   */
  it('resets the notice row without reaching the chat deliveries that share the class', () => {
    // The bare rule first: it is the one a later reader would write, and the
    // scoped rule below would throw before this ever ran. The match takes the
    // class wherever a member's first compound names it - `.notice`, a
    // variant, a type-dressed `div.notice` - and a scoped member, whose
    // `.notice` sits behind `.comp`, is not a member this rule asks about.
    expect(sheet, 'a bare notice rule strips chrome a delivery needs').not.toMatch(
      /(^|,)\s*[a-z]*\.notice[^,{}]*\{[^}]*padding:\s*0/m,
    );
    expect(sheetRule('.comp .notice'), 'the row does not state its own chrome').toMatch(
      /padding:\s*0/,
    );
  });

  /**
   * The check above has to see every rule that names the box, including the
   * ones that paint no border: the device it guards against is an inset
   * SHADOW, so a rule re-adding it would carry no border colour and an
   * iterator over the ring rules would walk straight past it.
   */
  it('sees an inset line on a rule for the box that paints no border', () => {
    const spliced = sheet.replace(
      '.surf.rec',
      '.surf.shadow { box-shadow: inset 0 -2px 0 var(--accent); }\n.surf.rec',
    );
    expect(insetRules(spliced), 'a borderless rule for the box draws the inset line').toEqual([
      '.surf.shadow',
    ]);
  });

  /**
   * The class name is bounded at both ends of a selector, so a neighbour's
   * class beginning with the same letters is not read as the box - reading it
   * would throw and take every test here with it.
   */
  it("does not read a neighbour's class as the box", () => {
    const spliced = sheet.replace(
      '.surf.rec',
      '.surfrow { border-color: var(--warn); }\n.surf.rec',
    );
    expect(ring(spliced, ['rec'], false), 'a class beginning with the same letters').toBe(
      'color-mix(in srgb, var(--accent) 35%, var(--hot))',
    );
  });

  /**
   * A grouped selector naming the box paints the box, wherever in the list it
   * sits: grouping is the sheet's own idiom, and a rule that named the box and
   * was skipped would be invisible to both checks above.
   */
  it('reads a grouped selector that names the box, first or second', () => {
    const colour = 'color-mix(in srgb, var(--accent) 35%, var(--hot))';
    for (const grouped of ['.surf.rec, .other {', '.other, .surf.rec {']) {
      const spliced = sheet.replace('.surf.rec {', grouped);
      expect(ring(spliced, ['rec'], false), `a rule grouped as ${JSON.stringify(grouped)}`).toBe(
        colour,
      );
    }
  });

  it('fails on a border shorthand it cannot read, rather than skipping it', () => {
    const spliced = sheet.replace(
      '.surf.rec',
      '.surf.odd { border: 1px solid var(--ok) inset; }\n.surf.rec',
    );
    expect(
      () => ring(spliced, ['rec'], false),
      'a border shorthand this resolver cannot read is skipped',
    ).toThrow(/cannot read/);
  });

  it('puts the controls at the right end of whichever row holds them', () => {
    // The bar is one row while the field is empty and the controls ride it;
    // a draft takes the second row and they follow it down. One rule serves
    // both, so neither row can drop them to the left of the field.
    expect(sheetRule('.ctls'), 'the controls lost the right end they take in either row').toContain(
      'margin-left: auto',
    );
    expect(() => sheetRule('.line .mic'), 'the mic still holds a column of the draft open').toThrow(
      /writes no rule/,
    );
  });

  /**
   * A narrow box cannot hold the hints and the controls on one row - measured,
   * the widest set needs 371px with a mouse's glyphs and 431px with a finger's
   * - so the keys go and the controls keep the row, the finger's set first.
   */
  it('keeps the key hints a pointer-only detail on the field row', () => {
    // The band cannot spare a row for the keys, and a finger has no use for
    // them, so they are the pointer's own detail on the field's line. Drawn
    // under a finger they cost the row its width; gone entirely, the rules
    // are a promise the field does not keep.
    expect(sheet, 'the hints are drawn under a finger').toMatch(
      /\.line \.hints \{ display: none; \}/,
    );
    expect(sheet, 'the hints never come back for a pointer that can see them').toMatch(
      /@media \(pointer: fine\) \{\n {2}\.line \.hints \{ display: inline-flex;/,
    );
  });

  /**
   * **The phone's floors are read on the block itself**, because the coarse
   * rules sit inside the media query, where the per-selector scan above
   * reaches only their base rules - and that is exactly how they went unpinned
   * while two changes crossed.
   */
  it("keeps the glyph controls and the panel's close at a finger's size, and the mic's accent", () => {
    // A rule per control rather than one selector pinned whole, so a control
    // that leaves the shared glyph rule for one of its own still has to state
    // both floors.
    const coarse = [...sheet.matchAll(/@media \(pointer: coarse\) \{\n([\s\S]*?)\n\}/g)]
      .map((match) => match[1])
      .join('\n');
    for (const control of ['mic', 'send', 'stop']) {
      const rule = new RegExp(`\\.${control}(?:, [^{]*)? \\{([^}]*)\\}`).exec(coarse)?.[1];
      expect(rule, `.${control} has no rule in a coarse-pointer block`).toBeDefined();
      expect(rule, `.${control} lost its height floor on a finger`).toContain('min-height: 44px');
      expect(rule, `.${control} lost its width floor on a finger`).toContain('min-width: 44px');
    }
    expect(coarse, "the panel's close lost its width floor").toContain(
      '.pop .hd .x { min-width: 44px; }',
    );
    // The fold control's floor lives in the dock's own multiline coarse block,
    // where the one-line pattern above cannot reach it: read from the media
    // query to the control's own line, the lines between allowed.
    expect(sheet, 'the fold control lost its width floor on a finger').toMatch(
      /@media \(pointer: coarse\) \{\n(?: {2}[^\n]*\n)*? {2}\.dock \.chevb \{ min-width: 44px; \}/,
    );
    expect(sheetRule('.mic:hover'), 'the mic lost its accent hover').toContain(
      'color: var(--accent)',
    );
  });

  it('keeps the separation the rows above the draft had before C moved the field and the footer', () => {
    expect(sheet, 'a row above the draft lost the 8px the box used to give it').toMatch(
      /\.comp \.notice \+ \.line[^{]*\{[^}]*margin-top: 8px/,
    );
  });

  it('centres the send in whatever box it gets, with no offset of its own', () => {
    const rule = sheetRule('.send');
    expect(rule, 'the send is not centred, so its icon tops out at the 44px phone floor').toContain(
      'place-items: center',
    );
    expect(
      rule,
      'the send carries an offset, which was a nudge for the alignment axis C moved',
    ).not.toContain('padding-top');
  });

  it('gives the controls the same pointer floor in either row', () => {
    for (const control of ['.mic', '.stop', '.send']) {
      const rule = sheetRule(control);
      expect(rule, `${control} is a bare glyph rather than a 24px target`).toContain('width: 24px');
      expect(rule, `${control} takes no height of its own`).toContain('height: 24px');
    }
  });

  it('draws no ring on the field, because the box is the only focus mark', () => {
    expect(
      sheetRule('.line .txt:focus'),
      'the field draws a second accent rectangle inside the box',
    ).toContain('outline: none');
  });
});

/**
 * The strip lane: the composer's surface is where the pin is drawn now.
 *
 * **A behaviour the column's tests used to pin**: the strip stands on a seat
 * with no conversation state to speak of - coming up, refused, not yet read,
 * empty - because the rows are the whole way into the seat's tree, tasks and
 * watchers. The column publishes the row through `pinnedRow` (see `Chat.test.ts
 * > the chat column as it draws`); this is where it draws, as the surface's
 * first lane.
 */
describe('the strip lane', () => {
  const SEAT = subjectKey({ session: SLOT });

  /** One running row, which is what the lane is handed. */
  const running: TurnInfo = {
    running: true,
    failed: false,
    duration_ms: 4200,
    api_ms: null,
    ended_at_utc: null,
    model: null,
    thinking_tokens: null,
    input_tokens: 700,
    output_tokens: 20,
    cache_read_tokens: null,
    cache_written_tokens: null,
    session_cost_usd: null,
  };

  afterEach(() => {
    pinnedRow.set(SEAT, null);
  });

  it("draws as the card's own first row, above the body", () => {
    pinnedRow.set(SEAT, running);
    open({});

    const row = document.querySelector('.live-row');
    const surf = document.querySelector('.surf');
    expect(row, 'the live row did not draw').not.toBeNull();
    expect(surf, 'the surface did not draw').not.toBeNull();
    if (row === null || surf === null) return;
    expect(
      row.nextElementSibling?.classList.contains('slot'),
      'the live row is not above the body slot',
    ).toBe(true);
    expect(row.textContent, 'the row the pin published').toContain('700');
  });

  it('stands on a seat with no conversation state to speak of', () => {
    // The composer draws it without any conversation at all: no page has
    // landed, nothing is running, and the lane is still there.
    pinnedRow.set(SEAT, running);
    open({});

    expect(document.querySelector('.live-row'), 'no strip lane on a bare seat').not.toBeNull();
  });

  it("shows one seat's pin only", () => {
    pinnedRow.set('session:somebody-else', running);
    open({});

    expect(
      document.querySelector('.live-row'),
      "another seat's pin drew on this surface",
    ).toBeNull();
  });

  it('draws no lane at all when nothing is pinned and no rows hold anything', () => {
    open({});

    expect(document.querySelector('.live-row'), 'a lane drew with nothing to say').toBeNull();
  });

  it('follows the seat when the page moves', () => {
    // The pin is read under the SHOWN seat: a page that moves must swap the
    // lanes with it, never keep drawing the seat being left's figures.
    const other: SessionSlot = { org: 'Busytools', project: 'forge', label: 'worker' };
    const otherKey = subjectKey({ session: other });
    pinnedRow.set(SEAT, running);
    pinnedRow.set(otherKey, { ...running, input_tokens: 42 });
    const harness = open({});

    expect(document.querySelector('.live-row')?.textContent, "the first seat's figures").toContain(
      '700',
    );

    harness.page.slot = other;
    flushSync();

    expect(
      document.querySelector('.live-row')?.textContent,
      'the figures did not follow the shown seat',
    ).toContain('42');
    pinnedRow.set(otherKey, null);
  });

  it('stands in every surface state: under a prompt, and on a blocked seat', () => {
    // The breadth the column's own test used to hold (four conversation
    // states): the lane is the surface's first child in EVERY arm - the
    // normal box, the prompt's lane, and the blocked line.
    pinnedRow.set(SEAT, running);
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });
    expect(
      document.querySelector('.surf .live-row'),
      'no strip lane under a prompt',
    ).not.toBeNull();

    harness.page.record = record();
    harness.page.seat = seatRead({ lifecycle: null, waking: true });
    flushSync();

    expect(document.querySelector('.dock'), 'precondition: the prompt is gone').toBeNull();
    expect(document.querySelector('.blocked'), 'precondition: the seat is blocked').not.toBeNull();
    expect(
      document.querySelector('.surf .live-row'),
      'no strip lane on a blocked seat',
    ).not.toBeNull();
  });
});

/**
 * **The card's body is one slot.** Exactly one of the field, an ask, a take's
 * card or a blocked line draws there, and the live row stands above it in
 * every one of those states. Two drawing at once is the stack this cut
 * removes.
 */
describe('the one body slot', () => {
  const KEY = subjectKey({ session: SLOT });
  afterEach(() => pinnedRow.set(KEY, null));

  it('draws exactly one of the field, an ask, a take and a blocked line', () => {
    // A pinned turn, so the row has something to say in every state below;
    // whether the row draws with nothing to say is its own test.
    pinnedRow.set(KEY, {
      running: true,
      failed: false,
      duration_ms: 0,
      api_ms: null,
      ended_at_utc: null,
      model: null,
      thinking_tokens: null,
      input_tokens: 1,
      output_tokens: null,
      cache_read_tokens: null,
      cache_written_tokens: null,
      session_cost_usd: null,
    });
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    expect(document.querySelector('.surf .slot .dock'), 'the ask holds the body').not.toBeNull();
    expect(document.querySelector('.surf .slot > .line'), 'and the field stands down').toBeNull();

    harness.page.record = record({ composer: { take: take(), notice: null, machine: null } });
    flushSync();
    expect(
      document.querySelector('.surf .slot > .line .tc'),
      'the take holds the body',
    ).not.toBeNull();
    expect(document.querySelector('.surf .slot .dock'), 'and the ask is gone').toBeNull();

    harness.page.seat = seatRead({ lifecycle: null, waking: true });
    flushSync();
    expect(
      document.querySelector('.surf .slot > .blocked'),
      'the blocked line holds the body',
    ).not.toBeNull();
    expect(document.querySelector('.surf .slot > .line'), 'and nothing else does').toBeNull();
    expect(
      document.querySelector('.surf .live-row'),
      'the live row stands above every one of them',
    ).not.toBeNull();
  });

  it("opens the autocomplete as the card's own popover, not a lane of the body", () => {
    // The list took the band from 121px to 557px while it sat in the box's
    // flow. It is a child of the card now, placed by a rule of its own: a
    // static child there is the menu back in the flow and the band growing
    // again.
    open();
    type('/');

    expect(
      document.querySelector('.surf > .ac'),
      "the list is not the card's child",
    ).not.toBeNull();
    expect(document.querySelector('.slot .ac'), 'the list went back into the body').toBeNull();
    expect(sheetRule('.surf > .ac'), 'the popover is back in the flow').toContain(
      'position: absolute',
    );
  });
});

/**
 * The dock's own round trip: what an answer carries, who has the keyboard, and
 * what happens to a take that is still running behind it.
 */
describe('the dock', () => {
  /** Every command the composer sent, in order. */
  const commands = (harness: { sent: { command: Record<string, unknown> }[] }) =>
    harness.sent.map((entry) => entry.command);

  it('draws the words row once, and typing in it turns no option on', () => {
    // The words are the answer's annotation, not a selection: the boxes a set
    // draws never see them, and the row they are written in is not one of the
    // options.
    open({ record: record({ pending_asks: [questionAsk()] }) });

    const field = document.querySelector<HTMLTextAreaElement>('.dock textarea.notes');
    if (field === null) throw new Error('the dock drew no words row');

    const boxes = (): boolean[] =>
      [...document.querySelectorAll('.dock .box2')].map((box) => box.classList.contains('on'));

    expect(boxes(), 'nothing typed, nothing checked').toEqual([false, false]);

    field.value = 'a teal, not listed';
    field.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();

    expect(boxes(), 'and typed words turn no option on').toEqual([false, false]);
  });

  it('draws a single-answer question as one pick, not a set of boxes', () => {
    // The regression this pins: every question drawn with the checkbox a SET
    // takes, so a question that accepts one row read as one that accepts many.
    open({ record: record({ pending_asks: [questionAsk('tu-q', { multi_select: false })] }) });

    expect(
      document.querySelectorAll('.dock .box2'),
      'a question that takes one row drew the boxes a set takes',
    ).toHaveLength(0);
    expect(
      document.querySelectorAll('.dock .opt .cur'),
      'and no mark stands in the slot the boxes hold',
    ).toHaveLength(0);
    expect(
      [...document.querySelectorAll('.dock .opt .radio')].map((dot) =>
        dot.classList.contains('on'),
      ),
      'the circle fills on the row being taken, and only there',
    ).toEqual([true, false]);
    expect(
      document.querySelectorAll('.dock .opt.sel'),
      'the marked row itself is what says which one is to be taken',
    ).toHaveLength(1);
  });

  it('stands down once its answer is on its way, and says so', () => {
    // The view this client replaced popped its prompt at submit; this dock
    // stands instead, with the mark the reader's own words carry while they are
    // out - so a second Enter is not read as a second answer to a prompt
    // already answered.
    const harness = open({
      record: record({ pending_asks: [questionAsk('tu-q', { multi_select: false })] }),
    });

    options()[1]?.click();
    flushSync();
    expect(commands(harness), 'the answer went').toHaveLength(1);
    expect(drawn(), 'and the dock says it is on its way').toContain('sending');

    options()[1]?.click();
    flushSync();
    expect(commands(harness), 'and nothing else can be sent from it').toHaveLength(1);
  });

  it('answers a question with the row that was clicked, and only that row', () => {
    const harness = open({
      record: record({ pending_asks: [questionAsk('tu-q', { multi_select: false })] }),
    });

    options()[1]?.click();
    flushSync();

    expect(commands(harness), 'a single-select answer is the clicked row').toEqual([
      {
        respond_question: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          tool_id: 'tu-q',
          outcome: {
            outcome: 'answered',
            selected_option_ids: ['q-prod'],
            annotation: null,
          },
        },
      },
    ]);
  });

  it('toggles the rows of a multi-select question, and answers with every row that is on', () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    options()[0]?.click();
    options()[1]?.click();
    flushSync();

    expect(
      [...document.querySelectorAll('.opt .box2')].map((box) => box.classList.contains('on')),
      'the boxes carry what is toggled',
    ).toEqual([true, true]);
    expect(commands(harness), 'a toggle is not an answer').toEqual([]);

    press('Enter');

    expect(commands(harness)).toEqual([
      {
        respond_question: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          tool_id: 'tu-q',
          outcome: {
            outcome: 'answered',
            selected_option_ids: ['q-staging', 'q-prod'],
            annotation: null,
          },
        },
      },
    ]);
  });

  it('toggles the marked row from the keyboard, which is the key its chip names', () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });
    const list = document.querySelector('.dock [role="listbox"]');
    if (!(list instanceof HTMLElement)) throw new Error('the dock drew no listbox');

    // The listbox is what holds the keys by default; a row an earlier click
    // focused is the other path, and it is reachable - `tabindex="-1"` takes
    // focus on click, just not by Tab. This leg is the listbox's.
    press(' ');
    expect(
      [...document.querySelectorAll('.dock .box2')].map((box) => box.classList.contains('on')),
      'space turns the marked row on',
    ).toEqual([true, false]);

    press(' ');
    expect(
      [...document.querySelectorAll('.dock .box2')].map((box) => box.classList.contains('on')),
      'and off again',
    ).toEqual([false, false]);
    expect(harness.sent, 'a toggle is never an answer').toEqual([]);
  });

  it('toggles once when the row itself holds the focus, rather than twice', () => {
    open({ record: record({ pending_asks: [questionAsk()] }) });

    // A browser focuses a `tabindex="-1"` row when it is clicked, so the row's
    // own key handler and the listbox's both see the next key: without the row
    // stopping it, one Space toggles the box on and the other straight back.
    // Focus is moved by hand because jsdom does not focus on click, and the
    // state under test is the one Chromium reaches.
    const row = options()[0];
    if (!(row instanceof HTMLElement)) throw new Error('the dock drew no options');
    row.focus();
    flushSync();
    expect(document.activeElement, 'the row is holding the keyboard').toBe(row);

    press(' ');

    expect(
      [...document.querySelectorAll('.dock .box2')].map((box) => box.classList.contains('on')),
      'the row turned on and stayed on',
    ).toEqual([true, false]);
  });

  it('keeps the keyboard with the mark, so a key after an arrow acts on the marked row', () => {
    open({ record: record({ pending_asks: [questionAsk()] }) });

    const row = options()[0];
    if (!(row instanceof HTMLElement)) throw new Error('the dock drew no options');
    row.focus();
    flushSync();

    press('ArrowDown');
    expect(document.querySelector('.dock .opt.sel')?.textContent, 'the mark moved').toContain(
      'Production',
    );
    expect(document.activeElement, 'and the keyboard moved with it').toBe(
      document.querySelector('.dock [role="listbox"]'),
    );

    press(' ');

    expect(
      [...document.querySelectorAll('.dock .box2')].map((box) => box.classList.contains('on')),
      'so the toggle lands on the row the reader can see is marked',
    ).toEqual([false, true]);
  });

  it('answers from Enter on a focused row, rather than only toggling it', () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    const row = options()[0];
    if (!(row instanceof HTMLElement)) throw new Error('the dock drew no options');
    row.focus();
    flushSync();

    press('Enter');

    expect(
      harness.sent.map((entry) => entry.command),
      'the keys line says Enter submits, and the row is where the keyboard is',
    ).toEqual([
      {
        respond_question: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          tool_id: 'tu-q',
          outcome: { outcome: 'answered', selected_option_ids: ['q-staging'], annotation: null },
        },
      },
    ]);
    expect(
      [...document.querySelectorAll('.dock .box2')].map((box) => box.classList.contains('on')),
      'Enter answers rather than toggling',
    ).toEqual([false, false]);
  });

  it('sends nothing when Enter lands in the words row with nothing said', () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    const field = document.querySelector('.dock textarea.notes');
    if (!(field instanceof HTMLElement)) throw new Error('the dock drew no words row');
    field.focus();
    flushSync();

    press('Enter');

    expect(harness.sent, 'nothing said is not an answer, and not a rejection').toEqual([]);
    expect(document.activeElement, 'and the caret stays where the reader is writing').toBe(field);
  });

  it('hands the keyboard back to the options from the words row', () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });
    const list = document.querySelector('.dock [role="listbox"]');
    if (!(list instanceof HTMLElement)) throw new Error('the dock drew no listbox');

    // The words row is a row like any other: the arrows have to keep working
    // from it, or it is a trap with no way back to the choices.
    const field = document.querySelector('.dock textarea.notes');
    if (!(field instanceof HTMLElement)) throw new Error('the dock drew no words row');
    field.focus();
    flushSync();

    press('ArrowUp');

    expect(document.activeElement, 'a key hands the keyboard back to the options').toBe(list);
    expect(harness.sent, 'moving the mark answers nothing').toEqual([]);
  });

  it('rejects a question on Escape, which is a way out the dock did not have', () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    press('Escape');

    expect(commands(harness)).toEqual([
      {
        respond_question: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          tool_id: 'tu-q',
          outcome: { outcome: 'cancelled' },
        },
      },
    ]);
  });

  it('rejects a question from its words row, rather than only from the options', () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    const field = document.querySelector('.dock textarea.notes');
    if (!(field instanceof HTMLElement)) throw new Error('the dock drew no words row');
    field.focus();
    flushSync();
    expect(document.activeElement, 'the words row is where the question takes words').toBe(field);

    press('Escape');

    expect(commands(harness), 'the same way out from both places').toEqual([
      {
        respond_question: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          tool_id: 'tu-q',
          outcome: { outcome: 'cancelled' },
        },
      },
    ]);
  });

  it('names the reject key on a question, which the keys line used to leave out', () => {
    open({ record: record({ pending_asks: [questionAsk()] }) });

    expect(drawn(), 'the keys line says what Escape does here').toContain('Esc reject');
  });

  it('draws a lone question with no batch marks, and a batch with its own', () => {
    const harness = open({ record: record({ pending_asks: [questionAsk('tu-q', {}, 0, 1)] }) });

    expect(
      document.querySelectorAll('.dock .segs i'),
      'the ordinary case carries no batch marks',
    ).toHaveLength(0);

    harness.page.record = record({ pending_asks: [questionAsk('tu-q', {}, 1, 3)] });
    flushSync();

    const marks = [...document.querySelectorAll('.dock .segs i')];
    expect(marks, 'a batch draws one mark per question').toHaveLength(3);
    expect(
      marks.findIndex((mark) => mark.classList.contains('on')),
      'with the one this question is lit, which is the mock the design settled on',
    ).toBe(1);
    expect(drawn(), 'and the position in words, which is what a screen reader reads').toContain(
      'question 2 of 3',
    );
  });

  it("draws an option's description under its name, where the terminal draws it", () => {
    open({ record: record({ pending_asks: [questionAsk()] }) });

    const row = options()[0];
    if (!(row instanceof HTMLElement)) throw new Error('the dock drew no options');
    const name = row.querySelector('.lbl');
    const vs = row.querySelector('.why');

    expect(vs?.textContent, 'the description is drawn').toBe('The pre-production cluster');
    expect(
      vs?.parentElement,
      'and it shares the name column, so the two align rather than the name being pushed around',
    ).toBe(name?.parentElement);
  });

  /**
   * **A preview is a document, so it is bounded and scrolls in place.** A
   * forty-line issue body used to grow the composer lane past the window, and
   * `.session-page` clips its grid - so the tail of the very thing being
   * approved was unreachable, with nothing on screen to say it was there. One
   * rule covers both panels, because both are the same region.
   *
   * Read off the sheet because the bound IS a declaration: jsdom lays out
   * nothing, so `scrollHeight > clientHeight` here would be asserting this
   * test's own stubs. The measured geometry at both widths rides the pull
   * request instead.
   */
  it('bounds a preview panel, so a long one scrolls rather than clipping', () => {
    // Off the working directory, which the suite runs from the client root:
    // jsdom's `import.meta.url` is an http one, so it cannot name the file.
    const sheet = readFileSync('src/assets/web.css', 'utf8').replace(/\/\*[\s\S]*?\*\//g, '');
    const rule = /\.dock \.preview\s*\{([^}]*)\}/.exec(sheet)?.[1];
    expect(rule, 'the sheet bounds the panel both previews are drawn in').toBeDefined();
    expect(rule, 'to a height the window decides').toMatch(/max-height:\s*min\([^)]*\)/);
    expect(rule, 'and gives it its own scroll').toMatch(/overflow-y:\s*(auto|scroll)/);
  });

  /**
   * **The preview owns the scroll keys while the keyboard is in it.** Tab is
   * how a reader with no wheel gets into the region, and the dock's own key
   * map would otherwise take the arrow to move the mark - so the tab stop
   * would scroll nothing and the rest of the document would stay unread.
   */
  it('keeps the scroll keys for the preview while the keyboard is in it', () => {
    const long = Array.from(
      { length: 40 },
      (_, at) => `paragraph ${String(at + 1)} of the preview`,
    ).join('\n\n');
    open({
      record: record({
        pending_asks: [
          questionAsk('tu-q', {
            options: [
              { option_id: 'q-long', label: 'The long one', description: null, preview: long },
              { option_id: 'q-prod', label: 'Production', description: null, preview: null },
            ],
          }),
        ],
      }),
    });

    const preview = document.querySelector('.dock .preview');
    if (!(preview instanceof HTMLElement)) throw new Error('the dock drew no preview region');
    expect(preview.tabIndex, 'the region is a tab stop, or no keyboard reaches it').toBe(0);
    expect(preview.getAttribute('role'), 'and it is named for what it is').toBe('region');
    expect(
      preview.getAttribute('aria-label'),
      "with the marked option's label, which is what a screen reader says over its prose",
    ).toBe("the marked option's preview");
    preview.focus();
    flushSync();

    press('ArrowDown');

    expect(
      options()[0]?.getAttribute('aria-selected'),
      'an arrow over the preview scrolls it rather than moving the mark',
    ).toBe('true');
    expect(document.activeElement, 'and the keyboard stays in the document being read').toBe(
      preview,
    );
  });

  /**
   * **A preview is line-structured ASCII, so it is drawn as its own source.**
   * A plan, a diff, a list of steps: a line beginning `+`, `-` or `#` is that
   * line, not a list item or a heading, and its indentation is its own. The
   * markdown renderer read all four as structure, which is the wrong document
   * rather than a wrapping of the right one (#1634).
   */
  it('draws a preview payload as its own source, not as markdown', () => {
    const payload = 'plan:\n+ added\n- removed\n# not a heading\n    indented';
    open({
      record: record({
        pending_asks: [
          questionAsk('tu-q', {
            options: [
              { option_id: 'q-plan', label: 'The plan', description: null, preview: payload },
            ],
          }),
        ],
      }),
    });

    const preview = document.querySelector('.dock .preview');
    if (!(preview instanceof HTMLElement)) throw new Error('the dock drew no preview region');
    expect(preview.textContent, 'the payload is drawn line for line').toBe(payload);
    expect(
      preview.querySelector('ul, ol, h1, h2, pre code'),
      'and no line of it is read as markdown',
    ).toBeNull();
  });

  /**
   * **The rest of the keys are still the dock's.** The region takes the keys
   * that scroll it and gives every other one back: a character typed while the
   * keyboard is in the preview is the first character of the reader's own
   * answer, and swallowing it there would break the type-to-start path the
   * dock has everywhere else.
   */
  it('still takes a character typed from the preview as the answer', () => {
    open({ record: record({ pending_asks: [questionAsk()] }) });

    const preview = document.querySelector('.dock .preview');
    if (!(preview instanceof HTMLElement)) throw new Error('the dock drew no preview region');
    preview.focus();
    flushSync();
    expect(
      document.activeElement,
      'the keyboard is in the region, or the dock would take the character without it',
    ).toBe(preview);

    press('k');

    const field = document.querySelector('.dock textarea.notes');
    if (!(field instanceof HTMLTextAreaElement)) throw new Error('the dock drew no words row');
    expect(field.value, 'the dock received the character').toBe('k');
    expect(document.activeElement, 'and the caret went where the character landed').toBe(field);
  });

  /**
   * **A link inside the region is the link's.** The region draws rendered
   * markdown, which can carry a link, and a reader tabbed onto one presses
   * Enter to follow it - the dock's own Enter would answer the marked row
   * instead. jsdom runs no native activation, so what these assertions pin is
   * the half this component owns: the dock must not swallow the key on its
   * way to the link. The region itself keeps the key, so the exemption is the
   * link rather than the panel: a guard on the panel would take Enter from
   * the reader who tabbed into it to answer.
   */
  it('answers the marked row on Enter in the preview region', () => {
    // **An option's preview is drawn as its own source** (#1634), so it holds
    // no link for Enter to land on - the region's own Enter is the row's
    // answer. (The held post keeps markdown, and its link rule has its own
    // case below.)
    const harness = open({
      record: record({
        pending_asks: [
          questionAsk('tu-q', {
            options: [
              { option_id: 'q-run', label: 'The runbook', description: null, preview: 'run it' },
              { option_id: 'q-prod', label: 'Production', description: null, preview: null },
            ],
          }),
        ],
      }),
    });

    const preview = document.querySelector('.dock .preview');
    if (!(preview instanceof HTMLElement)) throw new Error('the dock drew no preview region');
    expect(preview.querySelector('a'), 'the source preview draws no link').toBeNull();

    const onRegion = new KeyboardEvent('keydown', {
      key: 'Enter',
      bubbles: true,
      cancelable: true,
    });
    preview.dispatchEvent(onRegion);
    flushSync();

    expect(onRegion.defaultPrevented, 'the region answers its own Enter').toBe(true);
    expect(commands(harness), 'and answers the marked row').toEqual([
      {
        respond_question: {
          key: SLOT,
          tool_id: 'tu-q',
          outcome: { outcome: 'answered', selected_option_ids: ['q-run'], annotation: null },
        },
      },
    ]);
  });

  /**
   * **The held post's half of the link rule.** On a held post the old path
   * answered nothing either way, so a commands-only assertion would call the
   * broken dock fixed: what tells them apart is whether Enter was swallowed
   * on its way to the post's link.
   */
  it("does not swallow Enter on a held post's link", () => {
    const harness = open({
      record: record({
        pending_asks: [
          slackDraftAsk({
            text: 'Deploy finished. See [the runbook](https://example.com/runbook).',
          }),
        ],
      }),
    });

    const link = document.querySelector('.dock .preview a');
    if (!(link instanceof HTMLAnchorElement)) throw new Error('the held post drew no link');

    const event = new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true });
    link.dispatchEvent(event);
    flushSync();

    expect(event.defaultPrevented, "the post's link kept its Enter").toBe(false);
    expect(commands(harness), 'and nothing was answered on its way').toEqual([]);
  });

  /**
   * **The held post is the same document, so it is the same region.** A long
   * Slack post grew the composer lane exactly as a long option preview did,
   * and `.session-page` clips its grid - the words the reader is asked to
   * approve, with their tail unreachable and nothing on screen to say so.
   * Both panels are drawn by one snippet, so the post gets the very region the
   * option preview gets rather than a second copy of it.
   *
   * What the bound IS is a declaration in the sheet, asserted by the case
   * above; the overflow it produces is measured against the running page at
   * both widths, because jsdom lays out nothing and reading `scrollHeight`
   * against `clientHeight` here would assert this test's own stubs.
   */
  it('draws a held post in the bounded region an option preview uses', () => {
    const long = Array.from(
      { length: 40 },
      (_, at) => `paragraph ${String(at + 1)} of the post`,
    ).join('\n\n');
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    const option = document.querySelector('.dock .preview');
    if (!(option instanceof HTMLElement)) throw new Error('the dock drew no option preview');
    const shape = (el: HTMLElement) => ({
      tag: el.tagName,
      classes: el.className,
      role: el.getAttribute('role'),
      tabIndex: el.tabIndex,
      scrollRegion: el.hasAttribute('data-preview'),
    });

    harness.page.record = record({ pending_asks: [slackDraftAsk({ text: long })] });
    flushSync();

    const post = document.querySelector('.dock .preview');
    if (!(post instanceof HTMLElement)) throw new Error('the dock drew no held post region');
    expect(shape(post), 'the post gets the very panel the option preview gets').toEqual(
      shape(option),
    );
    expect(post.tabIndex, 'the region is a tab stop, or no keyboard reaches it').toBe(0);
    expect(post.getAttribute('role'), 'and it is named for what it is').toBe('region');
    expect(
      post.getAttribute('aria-label'),
      "with the post's own label, not the option preview's",
    ).toBe('the post held for approval');
    expect(
      drawn(),
      'the key line leads with the Tab that reaches the region, where the arrows scroll',
    ).toContain('Tab into the post, then ↑↓ scroll it');
    expect(drawn(), 'and names the second Tab for the two actions').toContain(
      "Tab again for Post and Don't send",
    );
  });

  /**
   * **The region takes only the keys that scroll it.** Escape is one it must
   * give back, and it is the held post's own dock key - "don't send" - which
   * makes it the key this kind of prompt can prove: a held post draws no words
   * row, so a typed character has nothing to land in, and the character path
   * is the option preview's case above.
   */
  it("lets a held post's Escape through the region to the dock's own don't send", () => {
    const harness = open({ record: record({ pending_asks: [slackDraftAsk()] }) });

    const post = document.querySelector('.dock .preview');
    if (!(post instanceof HTMLElement)) throw new Error('the dock drew no held post region');
    post.focus();
    flushSync();
    expect(
      document.activeElement,
      'the keyboard is in the region, or a dock key would pass this without it',
    ).toBe(post);

    press('Escape');

    expect(commands(harness), "the region gave the key back to the post's own dock").toEqual([
      {
        respond_slack_post: {
          key: SLOT,
          id: '0192e1c0-0000-7000-8000-000000000000',
          approved: false,
        },
      },
    ]);
  });

  it("offers the reader's own words as the agent's, which is the wording rule", () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    expect(
      document.querySelector('.dock textarea.notes')?.getAttribute('placeholder'),
      'the words row says agent, never the vendor',
    ).toContain('agent something else');

    // A permission's field is the name the core sent rather than this client's
    // own: the rule reaches that one where the name is written, not here, and a
    // local rename would put the dock's row out of step with the wire.
    harness.page.record = record({ pending_asks: [permissionAsk()] });
    flushSync();

    expect(
      document.querySelector('.dock textarea.notes')?.getAttribute('placeholder'),
      "so a permission draws the core's own name",
    ).toContain('agent something else');
  });

  it("does not deny on the reader's behalf from a permission's words row", () => {
    const harness = open({ record: record({ pending_asks: [permissionAsk()] }) });

    const notes = document.querySelector('.dock textarea.notes');
    if (!(notes instanceof HTMLElement)) throw new Error('the dock drew no words row');
    notes.focus();
    flushSync();
    press('Enter');

    expect(harness.sent, 'nothing said is nothing sent').toEqual([]);
  });

  it("carries the reader's own words as the answer's annotation, not as an option", () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    const notes = document.querySelector('.dock .notes');
    if (!(notes instanceof HTMLTextAreaElement)) {
      throw new Error('the dock drew no words row to write in');
    }
    notes.focus();
    notes.value = 'Also bump the queue worker concurrency';
    notes.dispatchEvent(new Event('input', { bubbles: true }));
    press('Enter');

    expect(commands(harness)).toEqual([
      {
        respond_question: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          tool_id: 'tu-q',
          outcome: {
            outcome: 'answered',
            selected_option_ids: [],
            annotation: { preview: null, notes: 'Also bump the queue worker concurrency' },
          },
        },
      },
    ]);
  });

  it('gives the words their own send, so an answer needs no option selected', () => {
    // The words-only answer used to be reachable only from the field's own
    // Enter, and every visible commit was selection-shaped - which read as a
    // selection being mandatory before the words could go.
    const harness = open({
      record: record({ pending_asks: [questionAsk('tu-q', { multi_select: false })] }),
    });

    expect(
      document.querySelector('.dock .sendb'),
      'nothing written yet, so there is nothing for the button to send',
    ).toBeNull();

    const notes = document.querySelector('.dock .notes');
    if (!(notes instanceof HTMLTextAreaElement)) throw new Error('no words row');
    notes.value = 'custom only';
    notes.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();

    const send = document.querySelector('.dock .sendb');
    if (!(send instanceof HTMLElement)) throw new Error('the send button did not appear');
    send.click();
    flushSync();

    expect(commands(harness)).toEqual([
      {
        respond_question: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          tool_id: 'tu-q',
          outcome: {
            outcome: 'answered',
            selected_option_ids: [],
            annotation: { preview: null, notes: 'custom only' },
          },
        },
      },
    ]);
  });

  it('carries words written before a row is clicked, beside that row', () => {
    const harness = open({
      record: record({ pending_asks: [questionAsk('tu-q', { multi_select: false })] }),
    });

    const notes = document.querySelector('.dock .notes');
    if (!(notes instanceof HTMLTextAreaElement)) throw new Error('no words row');
    notes.value = 'and afterwards';
    notes.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();

    options()[0]?.click();
    flushSync();

    expect(commands(harness), 'the row and the words ride together').toEqual([
      {
        respond_question: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          tool_id: 'tu-q',
          outcome: {
            outcome: 'answered',
            selected_option_ids: ['q-staging'],
            annotation: { preview: null, notes: 'and afterwards' },
          },
        },
      },
    ]);
  });

  it('sends toggled rows and the words together', () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    options()[0]?.click();
    flushSync();
    const notes = document.querySelector('.dock .notes');
    if (!(notes instanceof HTMLTextAreaElement)) throw new Error('no words row');
    notes.value = 'and afterwards';
    notes.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    notes.focus();
    flushSync();
    press('Enter');

    expect(commands(harness), 'the set and the words ride together').toEqual([
      {
        respond_question: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          tool_id: 'tu-q',
          outcome: {
            outcome: 'answered',
            selected_option_ids: ['q-staging'],
            annotation: { preview: null, notes: 'and afterwards' },
          },
        },
      },
    ]);
  });

  it('draws the words row past the last option, where arriving lands the caret', () => {
    const harness = open({
      record: record({ pending_asks: [questionAsk('tu-q', { multi_select: false })] }),
    });

    expect(
      [...document.querySelectorAll('.dock .opt .lbl')].map((held) => held.textContent),
      'the options are exactly what the wire sent',
    ).toEqual(['Staging', 'Production']);

    // The row past the last option IS the input: one step onto it lands the
    // caret, and the words alone are the answer.
    press('ArrowDown');
    press('ArrowDown');

    const notes = document.querySelector('.dock .notes');
    if (!(notes instanceof HTMLTextAreaElement)) throw new Error('no words row');
    expect(
      document.activeElement,
      'the row past the last option did not put the caret in the words',
    ).toBe(notes);
    expect(commands(harness), 'arriving answers nothing').toEqual([]);

    notes.value = 'custom only';
    notes.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    const send = document.querySelector('.dock .sendb');
    if (!(send instanceof HTMLElement)) throw new Error('no send button');
    send.click();
    flushSync();

    expect(commands(harness), 'the words alone are the answer').toEqual([
      {
        respond_question: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          tool_id: 'tu-q',
          outcome: {
            outcome: 'answered',
            selected_option_ids: [],
            annotation: { preview: null, notes: 'custom only' },
          },
        },
      },
    ]);
  });

  it('lands the caret on the words row with an ArrowUp off the first option', () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    // The words row is the last row: an ArrowUp from the first wraps to it,
    // and arriving there is what puts the caret in the field.
    press('ArrowUp');
    flushSync();

    const notes = document.querySelector('.dock .notes');
    expect(document.activeElement, 'the wrap did not put the caret in the words').toBe(notes);
    expect(commands(harness), 'arriving answers nothing').toEqual([]);
  });

  it("hands a permission's words to its deny, which carries them", () => {
    const harness = open({ record: record({ pending_asks: [permissionAsk()] }) });

    const notes = document.querySelector('.dock .notes');
    if (!(notes instanceof HTMLTextAreaElement)) {
      throw new Error('the dock drew no words row to write in');
    }
    notes.value = 'not this branch';
    notes.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();

    const deny = [...document.querySelectorAll('.dock .acts .btn')].find((button) =>
      button.textContent?.includes('Deny'),
    );
    if (!(deny instanceof HTMLElement)) throw new Error('the dock drew no deny');
    expect(deny.textContent, 'the deny says what it would carry').toContain('with these words');
    deny.click();
    flushSync();

    expect(commands(harness)).toEqual([
      {
        respond_permission: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          tool_id: 'tu-1',
          outcome: {
            outcome: 'selected',
            option_id: 'opt-notes',
            action: { kind: 'deny' },
            notes_text: 'not this branch',
          },
        },
      },
    ]);
  });

  it('takes the keyboard when a prompt takes the box, and gives it back when the prompt is gone', () => {
    const harness = open();
    type('keep this');

    harness.page.record = record({ pending_asks: [permissionAsk()] });
    flushSync();

    expect(document.activeElement, 'the dock owns the keyboard it was handed').toBe(
      document.querySelector('.dock'),
    );
    expect(
      document.querySelector('[data-editor="composer"]'),
      'and the composer is not mounted beside it',
    ).toBeNull();

    harness.page.record = record();
    flushSync();

    expect(document.activeElement, 'the box takes the keyboard back').toBe(field());
  });

  /**
   * A take lands where the reader is looking. While a prompt has the slot that
   * is the dock's own field - the composer draws no box of its own here - so
   * words written to the draft land on nothing at all.
   */
  it("puts a take's words in the box that is actually showing", async () => {
    const harness = open({
      dictation: true,
      record: record({ pending_asks: [questionAsk()] }),
    });

    /** The dock's own box, which is the one the reader opened. */
    const dockBox = (): HTMLTextAreaElement | null => {
      const found = document.querySelector('.dock [data-editor="dock"]');
      return found instanceof HTMLTextAreaElement ? found : null;
    };

    expect(dockBox()?.value, 'the box the reader opened starts empty').toBe('');

    // The reader is in the row, which is what makes this dock the destination.
    const row = dockBox();
    if (row === null) throw new Error('the dock drew no words row');
    row.focus();
    flushSync();

    // The take begins while the dock holds the slot, which is where its words
    // were spoken to go.
    await beginTake(harness);
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'the words', truncated: false },
        machine: null,
      },
    });
    flushSync();

    expect(dockBox()?.value, 'the words land in the dock, because that is the box on screen').toBe(
      'the words',
    );
    expect(
      document.querySelector('[data-editor="composer"]'),
      'and the composer is not mounted at all while a prompt is up',
    ).toBeNull();
  });

  /**
   * **The dictation the dock's row never touched still belongs to the dock.**
   * A take started by a keyboard shortcut, with a question holding the slot,
   * used to land its words in the composer's draft - a box that is not even
   * drawn while the prompt is up - so the answer could never carry them.
   */
  it("lands a take's words in the dock when the take began under the question", async () => {
    const harness = open({
      dictation: true,
      record: record({ pending_asks: [questionAsk()] }),
    });

    await beginTake(harness);
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'custom words', truncated: false },
        machine: null,
      },
    });
    flushSync();

    const dockBox = document.querySelector('.dock [data-editor="dock"]');
    expect(
      dockBox instanceof HTMLTextAreaElement ? dockBox.value : null,
      'the words landed in the box the prompt is drawing',
    ).toBe('custom words');
  });

  /**
   * **The destination is captured per TAKE, not once per box.** One seat takes
   * several in a row, and a capture that never re-runs hands the second take
   * the first one's answer - the headline defect, from take two on.
   */
  it('gives each take its own destination: a later take under the question lands there', async () => {
    const harness = open({ dictation: true });

    // Take one begins with no prompt, so its words belong to the draft.
    await beginTake(harness);
    harness.page.record = record({
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'first words', truncated: false },
        machine: null,
      },
    });
    flushSync();

    // Then a question arrives, and take two begins under it.
    harness.page.record = record({ pending_asks: [questionAsk()] });
    flushSync();
    await beginTake(harness);
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'second words', truncated: false },
        machine: null,
      },
    });
    flushSync();

    const dockBox = document.querySelector('.dock [data-editor="dock"]');
    expect(
      dockBox instanceof HTMLTextAreaElement ? dockBox.value : null,
      'the second take landed where IT began, not where the first one did',
    ).toBe('second words');
  });

  it('gives each take its own destination: a later take begun in the composer stays there', async () => {
    const harness = open({
      dictation: true,
      record: record({ pending_asks: [questionAsk()] }),
    });

    // Take one begins under the question and lands in its row.
    await beginTake(harness);
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'first words', truncated: false },
        machine: null,
      },
    });
    flushSync();

    // The question resolves, and take two begins in the composer.
    harness.page.record = record();
    flushSync();
    await beginTake(harness);

    // A question arrives mid-flight; this take's words are still the draft's.
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: { take: take(), notice: null, machine: null },
    });
    flushSync();
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'second words', truncated: false },
        machine: null,
      },
    });
    flushSync();

    const dockBox = document.querySelector('.dock [data-editor="dock"]');
    expect(
      dockBox instanceof HTMLTextAreaElement ? dockBox.value : null,
      'a prompt arriving mid-take did not take words spoken for the draft',
    ).toBe('');

    harness.page.record = record();
    flushSync();
    expect(field().value, 'the words are in the draft the take was spoken into').toContain(
      'second words',
    );
  });

  /**
   * **A take can end while the reader is on another seat.** The clear has to
   * run for that take too: a flag left open swallows the next take's capture,
   * and that take lands by the one before it.
   */
  it('captures the next take after one ended while another seat was shown', async () => {
    const harness = open({ dictation: true });

    // Take one begins with no prompt, so its words belong to the draft.
    await beginTake(harness);

    // The reader moves to another seat, and the take ends while they are away.
    harness.page.slot = ELSEWHERE;
    flushSync();
    harness.page.record = record({
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'first words', truncated: false },
        machine: null,
      },
    });
    flushSync();

    // They come back with a question up, and take two begins under it: the
    // destination is captured per take, so the one before it cannot hand
    // this one the prompt that arrived in between.
    harness.page.slot = SLOT;
    harness.page.record = record({ pending_asks: [questionAsk()] });
    flushSync();
    await beginTake(harness);
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'second words', truncated: false },
        machine: null,
      },
    });
    flushSync();

    const dockBox = document.querySelector('.dock [data-editor="dock"]');
    expect(
      dockBox instanceof HTMLTextAreaElement ? dockBox.value : null,
      'the take after the unwatched one landed where it began',
    ).toBe('second words');
  });

  /**
   * **A prompt that resolves before the words land is not the destination.**
   * The capture says where the take began; the landing also checks what is
   * still on screen - a dock that has gone leaves its words to the draft.
   */
  it('lands in the draft when the question resolved before the take did', async () => {
    const harness = open({
      dictation: true,
      record: record({ pending_asks: [questionAsk()] }),
    });

    await beginTake(harness);
    harness.page.record = record({
      composer: { take: take(), notice: null, machine: null },
    });
    flushSync();
    harness.page.record = record({
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'orphan words', truncated: false },
        machine: null,
      },
    });
    flushSync();

    expect(field().value, 'the words went to the box that still exists').toContain('orphan words');
  });

  /**
   * **The cut row belongs to the box the words went into.** A truncated take
   * begun under the dock leaves its words in the dock's own row, and when the
   * prompt resolves the composer's draft never held them - so a warn row over
   * that draft is a note about words that are not there (the gate's round
   * three, 2026-10-15).
   */
  it("draws no cut row over a draft the dock's take never filled", async () => {
    const harness = open({
      dictation: true,
      record: record({ pending_asks: [questionAsk()] }),
    });

    await beginTake(harness);
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'this is what fitted', truncated: true },
        machine: null,
      },
    });
    flushSync();

    const dockBox = document.querySelector('.dock [data-editor="dock"]');
    expect(
      dockBox instanceof HTMLTextAreaElement ? dockBox.value : null,
      'the words went into the dock, where the take began',
    ).toBe('this is what fitted');

    // The prompt resolves, the notice still on the record.
    harness.page.record = record({
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'this is what fitted', truncated: true },
        machine: null,
      },
    });
    flushSync();

    expect(field().value, 'the draft never held those words').toBe('');
    expect(
      document.querySelector('.notice.warn'),
      'a cut row over a draft the words never reached',
    ).toBeNull();
  });

  it('lands in the draft under a permission that offers no words row', async () => {
    const bare = permissionAsk('tu-1', {
      options: [
        { option_id: 'opt-once', name: 'Allow once', kind: 'allow', action: { kind: 'allow' } },
        { option_id: 'opt-deny', name: 'Deny', kind: 'deny', action: { kind: 'deny' } },
      ],
    });
    const harness = open({ dictation: true, record: record({ pending_asks: [bare] }) });

    await beginTake(harness);
    harness.page.record = record({
      pending_asks: [bare],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'draft words', truncated: false },
        machine: null,
      },
    });
    flushSync();

    harness.page.record = record();
    flushSync();
    expect(field().value, 'no words row, so the draft is the box').toContain('draft words');
  });

  it('hides the send while a take draws in the words row', () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    const notes = document.querySelector('.dock .notes');
    if (!(notes instanceof HTMLTextAreaElement)) throw new Error('no words row');
    notes.value = 'words';
    notes.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    expect(document.querySelector('.dock .sendb'), 'words typed, a send to go').not.toBeNull();

    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: { take: take(), notice: null, machine: null },
    });
    flushSync();
    expect(
      document.querySelector('.dock .sendb'),
      'the take draws its own controls, not the send',
    ).toBeNull();

    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'more', truncated: false },
        machine: null,
      },
    });
    flushSync();
    expect(
      document.querySelector('.dock .sendb'),
      'the card is gone, the send returns',
    ).not.toBeNull();
  });

  /**
   * The dock is a destination only while its box is on screen. A seat can fail
   * with a prompt still waiting, which puts the blocker in the slot and takes
   * the dock away with it - and a landing then has no box of the dock's to go
   * to, so the words belong to the draft, which is held across the morph.
   */
  it("keeps a take's words when the blocker takes the dock away before they land", async () => {
    const harness = open({
      dictation: true,
      record: record({ pending_asks: [questionAsk()] }),
    });

    expect(
      document.querySelector('.dock [data-editor="dock"]'),
      'the dock draws the box the words would go in',
    ).not.toBeNull();

    harness.page.seat = seatRead({ lifecycle: 'Failed', reason: 'the CLI exited with status 1' });
    flushSync();
    expect(document.querySelector('.dock'), 'and then the dock is gone').toBeNull();

    // The take begins with no dock on screen, so the draft is its destination.
    await beginTake(harness);
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: { take: take(), notice: null, machine: null },
    });
    flushSync();
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'the words', truncated: false },
        machine: null,
      },
    });
    flushSync();

    harness.page.seat = seatRead();
    harness.page.record = record();
    flushSync();

    expect(
      field().value,
      'the words are still the reader to send, because no dock was there to take them',
    ).toBe('the words');
  });

  /**
   * What was written in a prompt's own-words box goes with that prompt.
   *
   * The box's draft is the composer's state now rather than the dock's, so the
   * prompt unmounting no longer takes it - which makes this the one leg of that
   * hoist with nothing under it.
   */
  it("opens the next prompt's own-words box empty, whatever was written in the last one", () => {
    const harness = open({ record: record({ pending_asks: [questionAsk('tu-q')] }) });

    /** The words row of whatever dock is up, which is always drawn. */
    const ownWords = (): HTMLTextAreaElement => {
      const box = document.querySelector('.dock [data-editor="dock"]');
      if (!(box instanceof HTMLTextAreaElement)) throw new Error('the dock drew no words row');
      return box;
    };

    const box = ownWords();
    box.value = 'leftover words';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();

    harness.page.record = record();
    flushSync();
    harness.page.record = record({ pending_asks: [questionAsk('tu-q2')] });
    flushSync();

    expect(ownWords().value, "the last prompt's words do not come back in this one").toBe('');
  });

  /**
   * The same guarantee when the next prompt arrives in the same frame as the one
   * before it, with no frame in between where nothing is asking.
   *
   * A page re-render is one assignment, so the harness reaches this shape even
   * though driving the CLI into it is a different question - and a release keyed
   * on the ask being absent cannot see it.
   */
  it("opens the next prompt's own-words box empty when the prompts change in one frame", () => {
    const harness = open({ record: record({ pending_asks: [questionAsk('tu-q')] }) });

    const ownWords = (): HTMLTextAreaElement => {
      const box = document.querySelector('.dock [data-editor="dock"]');
      if (!(box instanceof HTMLTextAreaElement)) throw new Error('the dock drew no words row');
      return box;
    };

    const box = ownWords();
    box.value = 'leftover words';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();

    // One frame, and the next prompt is already asking.
    harness.page.record = record({ pending_asks: [questionAsk('tu-q2')] });
    flushSync();

    expect(ownWords().value, "the last prompt's words do not come back in this one").toBe('');
  });

  /**
   * One tool call carries every question of a batch - the core reuses the tool
   * id and advances only the index - so two questions of one call are two
   * prompts, and the box question one was written in is not question two's.
   */
  it("opens the next question's own-words box empty when one call carries two", () => {
    const harness = open({ record: record({ pending_asks: [questionAsk('tu-q', {}, 0, 2)] }) });

    const ownWords = (): HTMLTextAreaElement => {
      const box = document.querySelector('.dock [data-editor="dock"]');
      if (!(box instanceof HTMLTextAreaElement)) throw new Error('the dock drew no words row');
      return box;
    };

    const box = ownWords();
    box.value = 'words meant for question one';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();

    // Answered, and the batch moves to its second question under the same call.
    press('Enter');
    harness.page.record = record({ pending_asks: [questionAsk('tu-q', {}, 1, 2)] });
    flushSync();

    expect(ownWords().value, "question one's words do not come back in question two's box").toBe(
      '',
    );
  });

  /**
   * A parallel batch parks two asks at once, and the queue draws the front.
   *
   * Two AskUserQuestion calls in ONE assistant message carry different tool
   * ids, so the record holds both while the reader answers the first. Its
   * resolution leaves the second as the front, drawn live: the stand-down is
   * keyed on the answered prompt itself, so it cannot outlive it and swallow
   * the ask behind.
   */
  it('draws the second of a parallel pair once the first is answered', () => {
    const harness = open({
      record: record({
        pending_asks: [
          questionAsk('tu-a', { multi_select: false }),
          questionAsk('tu-b', { multi_select: false }),
        ],
      }),
    });

    options()[0]?.click();
    flushSync();

    expect(commands(harness), 'the front ask is the one an answer names').toEqual([
      {
        respond_question: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          tool_id: 'tu-a',
          outcome: { outcome: 'answered', selected_option_ids: ['q-staging'], annotation: null },
        },
      },
    ]);
    expect(
      drawn(),
      'and the answered ask holds the dock down while its resolution is on the way',
    ).toContain('sending');

    // The resolution lands: the record falls to the ask that was behind.
    harness.page.record = record({ pending_asks: [questionAsk('tu-b', { multi_select: false })] });
    flushSync();

    expect(drawn(), 'the ask behind is drawn live, not swallowed by the stand-down').not.toContain(
      'sending',
    );

    options()[0]?.click();
    flushSync();

    expect(commands(harness).at(-1), 'and an answer names it').toEqual({
      respond_question: {
        key: { org: 'Busytools', project: 'forge', label: 'lead' },
        tool_id: 'tu-b',
        outcome: { outcome: 'answered', selected_option_ids: ['q-staging'], annotation: null },
      },
    });
  });

  /**
   * The other half of that release: it is keyed on the prompt's identity, so a
   * frame carrying the same question must leave the box alone. A key that moved
   * on every render would clear the box under the reader, which is worse than
   * the leak it fixes.
   */
  it('keeps what the reader wrote when the same prompt arrives again', () => {
    const harness = open({ record: record({ pending_asks: [questionAsk('tu-q')] }) });

    const ownWords = (): HTMLTextAreaElement => {
      const box = document.querySelector('.dock [data-editor="dock"]');
      if (!(box instanceof HTMLTextAreaElement)) throw new Error('the dock drew no words row');
      return box;
    };

    const box = ownWords();
    box.value = 'keep me';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();

    // The same question as a fresh frame carries it: a new object, one id.
    harness.page.record = record({ pending_asks: [questionAsk('tu-q')] });
    flushSync();

    expect(ownWords().value, 'a re-render does not clear the box under the reader').toBe('keep me');
  });

  /**
   * And it does not take the caret either.
   *
   * The record is replaced on every frame and on the session poll, so `ask` is
   * a fresh object while the prompt is the same one. The dock took the keyboard
   * again on each of those, keyed on the object rather than on the prompt: a
   * reader typing in the notes row lost the caret to the option list
   * mid-sentence, and the rest of their typing went to the listbox.
   */
  it('keeps the caret in the own-words box when the same prompt re-renders', () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    const own = document.querySelector('.dock textarea.notes');
    if (!(own instanceof HTMLElement)) throw new Error('the dock drew no words row');
    own.focus();
    flushSync();

    const box = document.querySelector('.dock [data-editor="dock"]');
    if (!(box instanceof HTMLTextAreaElement)) throw new Error('the own-words row drew no box');
    expect(document.activeElement, 'the box the row opened holds the keyboard').toBe(box);
    box.value = 'half a sentence';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();

    // The same question as a fresh frame carries it: a new object, one id.
    harness.page.record = record({ pending_asks: [questionAsk()] });
    flushSync();

    expect(document.activeElement, 'the same prompt re-drawn does not take the caret').toBe(box);
    expect(box.value, 'and the half-typed sentence is still there').toBe('half a sentence');
  });

  /**
   * The words come with the keyboard, and while a prompt has the slot the box
   * they land in is the dock's own. Opening that box puts the keyboard in it,
   * so this is the state left after the reader has clicked away from it.
   */
  it("brings the keyboard back to the dock's box when a take lands in it", async () => {
    const harness = open({
      dictation: true,
      record: record({ pending_asks: [questionAsk()] }),
    });

    const own = document.querySelector('.dock textarea.notes');
    if (!(own instanceof HTMLElement)) throw new Error('the dock drew no words row');
    own.focus();
    flushSync();

    const box = (): HTMLTextAreaElement | null => {
      const found = document.querySelector('.dock [data-editor="dock"]');
      return found instanceof HTMLTextAreaElement ? found : null;
    };
    expect(document.activeElement, 'opening the box puts the keyboard in it').toBe(box());

    // The reader clicks somewhere else, which is the state the take finds them in.
    box()?.blur();
    flushSync();
    expect(document.activeElement, 'and the keyboard is elsewhere now').not.toBe(box());

    await beginTake(harness);
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'the words', truncated: false },
        machine: null,
      },
    });
    flushSync();

    expect(box()?.value, 'the words land in the box that was open').toBe('the words');
    expect(document.activeElement, 'and they bring the keyboard with them').toBe(box());
  });

  /**
   * The keys a prompt's own box answers to are the box's own, and that routing
   * is by the editor the box names rather than by its class - a restyle that
   * renamed the class would otherwise reroute them in silence.
   */
  it('hands the keyboard back to the dock when Escape lands in the words row', () => {
    const harness = open({ record: record({ pending_asks: [permissionAsk()] }) });

    const own = document.querySelector('.dock textarea.notes');
    if (!(own instanceof HTMLElement)) throw new Error('the dock drew no words row');
    own.focus();
    flushSync();

    const box = document.querySelector('.dock [data-editor="dock"]');
    expect(document.activeElement, 'the words row holds the keyboard').toBe(box);

    press('Escape');

    expect(
      document.activeElement,
      'and a permission steps back out of the field rather than denying on the reader',
    ).not.toBe(box);
    // The distinction the routing turns on: a key the dock reads as the field's
    // own moves the mark, and the same key read as the dock's answers the
    // prompt - which for a permission is a deny nobody asked for.
    expect(harness.sent, 'a key in the field does not answer the prompt').toEqual([]);
  });

  /**
   * **Escape in the dock does not interrupt the parked turn.** A parked ask
   * holds the turn open - `turn_in_flight` is true exactly then - and the
   * turn's Escape rung must decline it: interrupting would kill the prompt
   * and the words the reader typed into it. Same press, same nothing sent.
   */
  it('leaves the parked turn alone when Escape steps out of the dock', () => {
    const harness = open({
      record: record({ header: { turn_in_flight: true }, pending_asks: [permissionAsk()] }),
    });

    const own = document.querySelector('.dock textarea.notes');
    if (!(own instanceof HTMLElement)) throw new Error('the dock drew no words row');
    own.focus();
    flushSync();

    press('Escape');

    expect(
      harness.sent,
      'the parked prompt and its words survive: this turn is not the reader to stop',
    ).toEqual([]);

    // And with the keyboard anywhere else - the dock up but nothing in it
    // focused - the dock's own key answers the prompt, and the rung STILL
    // declines: no interrupt may ride that press.
    own.blur();
    flushSync();
    harness.sent.length = 0;
    press('Escape');
    expect(
      harness.sent.some(
        (sent) => 'command' in sent && 'cancel' in (sent.command as Record<string, unknown>),
      ),
      'still no interrupt, focus or not: the parked turn is not the reader to stop',
    ).toBe(false);
  });

  /**
   * Shift+Enter is the box's own, which is the one key the fall-through gets
   * wrong in the other direction: the field's branch checks the shift and the
   * dock's does not, so a newline reaches `submit` and the prompt is answered
   * with words the reader was still writing.
   */
  it('keeps Shift+Enter the box own, so a newline does not answer the prompt', () => {
    const harness = open({ record: record({ pending_asks: [permissionAsk()] }) });

    const own = document.querySelector('.dock textarea.notes');
    if (!(own instanceof HTMLElement)) throw new Error('the dock drew no words row');
    own.focus();
    flushSync();

    const box = document.querySelector('.dock [data-editor="dock"]');
    if (!(box instanceof HTMLTextAreaElement)) throw new Error('the own-words row drew no box');
    // Words already in the box, because an empty one is refused rather than
    // sent - the harm is a half-written answer going out.
    box.value = 'not this branch';
    box.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();

    box.dispatchEvent(
      new KeyboardEvent('keydown', {
        key: 'Enter',
        shiftKey: true,
        bubbles: true,
        cancelable: true,
      }),
    );
    flushSync();

    expect(harness.sent, 'a newline in the box is not an answer').toEqual([]);
    expect(
      document.querySelector('.dock [data-editor="dock"]'),
      'and the box is still there to write in',
    ).not.toBeNull();
  });

  it('answers the next question with its own rows, not the ones the last one left', () => {
    const harness = open({
      record: record({ pending_asks: [oneOf(['q1-a', 'q1-b', 'q1-c', 'q1-d'], 0)] }),
    });

    // A row turned on for question one, and the mark left four rows down it.
    options()[3]?.click();
    flushSync();
    expect(
      [...document.querySelectorAll('.dock .box2')].map((box) => box.classList.contains('on')),
      'question one has its fourth row on',
    ).toEqual([false, false, false, true]);

    harness.page.record = record({ pending_asks: [oneOf(['q2-a', 'q2-b'], 1)] });
    flushSync();

    press('Enter');

    // This shape exercises the mark: it sat past the question's rows, so on a
    // dock that carried it over nothing would draw as marked and Enter would
    // answer with nothing until the reader arrowed. A fresh dock draws its own
    // first row marked instead, which is where the answer below comes from - and
    // the test beneath this one is the one that reaches the toggle.
    expect(commands(harness), 'Enter answers with the row this question drew').toEqual([
      {
        respond_question: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          tool_id: 'tu-q',
          outcome: { outcome: 'answered', selected_option_ids: ['q2-a'], annotation: null },
        },
      },
    ]);
  });

  /**
   * The toggle-only path, which the test above cannot reach: there the mark sat
   * past the question's rows, so nothing was dispatched for the toggle to show
   * itself in. Here the row turned on for question one is the one whose id
   * question two also draws, so a toggle that carried over would be drawn as on
   * for a question the reader has not answered.
   */
  it("draws the next question with nothing turned on, when a row shares the last one's id", () => {
    const harness = open({
      record: record({ pending_asks: [oneOf(['question_0', 'question_1'], 0)] }),
    });

    // The second row, whose id is the one question two draws first: turning on
    // any other would leave the id shared with nothing and the check below
    // holding whatever the toggle did.
    options()[1]?.click();
    flushSync();
    expect(
      [...document.querySelectorAll('.dock .box2')].map((box) => box.classList.contains('on')),
      'question one has its second row on',
    ).toEqual([false, true]);

    harness.page.record = record({ pending_asks: [oneOf(['question_1', 'question_2'], 1)] });
    flushSync();

    expect(
      [...document.querySelectorAll('.dock .box2')].map((box) => box.classList.contains('on')),
      'and the question the reader has not answered draws with nothing on',
    ).toEqual([false, false]);
  });

  it('keeps what the reader turned on through a repaint of the same question', () => {
    const harness = open({
      record: record({ pending_asks: [oneOf(['q1-a', 'q1-b'], 0)] }),
    });

    options()[0]?.click();
    flushSync();

    // The same question as a fresh frame carries it: a new object, one key.
    harness.page.record = record({ pending_asks: [oneOf(['q1-a', 'q1-b'], 0)] });
    flushSync();

    expect(
      [...document.querySelectorAll('.dock .box2')].map((box) => box.classList.contains('on')),
      'a repaint does not clear what the reader turned on',
    ).toEqual([true, false]);
  });

  it('moves the mark from the keyboard once the dock has the slot', () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });
    const list = document.querySelector('.dock [role="listbox"]');
    if (!(list instanceof HTMLElement)) throw new Error('the dock drew no listbox');

    list.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true }));
    flushSync();

    expect(document.querySelector('.opt.sel')?.textContent).toContain('Production');
    expect(
      list.getAttribute('aria-activedescendant'),
      'and the listbox points at the row it moved to',
    ).toContain('q-prod');

    // One more step wraps onto the words row, which is NOT an option: the
    // attribute has to be absent there, or a screen reader keeps hearing an
    // option while the caret is in the input.
    press('ArrowUp');
    press('ArrowUp');
    expect(
      list.getAttribute('aria-activedescendant'),
      'the listbox still points at an option while the mark is on the words row',
    ).toBeNull();
    void harness;
  });

  it('keeps a running take visible while a prompt holds the slot, and abandons it on escape', () => {
    const harness = open({
      record: record({
        pending_asks: [permissionAsk()],
        composer: { take: take(), notice: null, machine: null },
      }),
    });

    expect(
      document.querySelector('.dock .qrow .tc'),
      'the take draws on the row its words land in, rather than being swallowed',
    ).not.toBeNull();

    const dock = document.querySelector('.dock');
    if (!(dock instanceof HTMLElement)) throw new Error('the dock drew nothing');
    dock.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    flushSync();

    expect(commands(harness), 'the first escape belongs to the take').toEqual([
      {
        dictate_stop: { key: { org: 'Busytools', project: 'forge', label: 'lead' }, submit: false },
      },
    ]);
  });

  it('leaves a refused send in the column, and says why, rather than refilling the box', () => {
    const harness = open();
    type('push it once CI is green');
    press('Enter');
    expect(field().value, 'the box is cleared for the next thing').toBe('');

    harness.say({ kind: 'error', what: 'dispatch', why: 'the session is not running' });
    flushSync();

    expect(field().value, 'the box is not refilled over whatever was typed since').toBe('');
    expect(drawn(), 'and the box draws no line of its own about it').not.toContain(
      'the session is not running',
    );
    expect(echoAt(SLOT), 'the row carrying the words is the one that says why').toMatchObject({
      state: 'failed',
      words: 'push it once CI is green',
      why: 'the session is not running',
    });
  });

  it('draws a held post as the terminal draws it: where it goes, and the words in full', () => {
    open({ record: record({ pending_asks: [slackDraftAsk()] }) });

    expect(drawn()).toContain('Post to Slack');
    expect(drawn()).toContain('Trust Machines · granite-staging-alerts');
    expect(drawn(), 'the words being approved are shown in full').toContain(
      'Deploy finished on staging.',
    );
    expect(drawn(), 'and does not claim options this view never drew').not.toContain(
      'arrived before this view attached',
    );
  });

  it('names the thread a reply goes into, rather than the tool that composed it', () => {
    open({ record: record({ pending_asks: [slackDraftAsk({ thread_ts: '1758901234.482910' })] }) });

    expect(drawn()).toContain('Reply in Slack');
    expect(drawn()).toContain('thread 1758901234.482910');
    expect(drawn(), 'the dock says where it goes, not which tool asked').not.toContain(
      'slack__post',
    );
  });

  it('posts the held draft from the verb the reader picks, addressed by its own id', () => {
    const harness = open({ record: record({ pending_asks: [slackDraftAsk()] }) });

    action('Post').click();
    flushSync();

    expect(commands(harness)).toEqual([
      {
        respond_slack_post: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          id: '0192e1c0-0000-7000-8000-000000000000',
          approved: true,
        },
      },
    ]);
  });

  it('says why when the core refuses the draft, rather than dropping it in silence', () => {
    const harness = open({ record: record({ pending_asks: [slackDraftAsk()] }) });

    action('Post').click();
    flushSync();

    harness.say({ kind: 'error', what: 'dispatch', why: 'the connector is not configured' });
    flushSync();

    expect(document.querySelector('.dock'), 'the dock comes back with the refusal').not.toBeNull();
    expect(drawn()).toContain('the connector is not configured');
  });

  it('clears the refusal it showed when the reader answers the draft again', () => {
    const harness = open({ record: record({ pending_asks: [slackDraftAsk()] }) });

    action('Post').click();
    flushSync();
    harness.say({ kind: 'error', what: 'dispatch', why: 'the connector is not configured' });
    flushSync();

    // The retry is the only thing the reader can do about a refusal, and the
    // reason belonged to the attempt that failed: left standing it reads as a
    // verdict on this one.
    action('Post').click();
    flushSync();

    expect(commands(harness), 'the answer went out again').toHaveLength(2);
    expect(drawn(), "and the first failure's reason goes with it").not.toContain(
      'the connector is not configured',
    );
    expect(document.querySelector('.dock'), 'the dock goes with the answer').toBeNull();
  });

  it('takes the dock away once the draft is answered, before any frame says so', () => {
    const harness = open({ record: record({ pending_asks: [slackDraftAsk()] }) });

    action('Post').click();
    flushSync();

    expect(commands(harness), 'the approval went out').toHaveLength(1);
    expect(document.querySelector('.dock'), 'and the dock goes with it').toBeNull();
    expect(
      document.querySelector('[data-editor="composer"]'),
      'the box takes the slot back',
    ).not.toBeNull();
  });

  it('does not raise an answered draft again, while the next one still draws', () => {
    const harness = open({ record: record({ pending_asks: [slackDraftAsk()] }) });

    action('Post').click();
    flushSync();

    // A frame read before the resolution still carries the draft - the seat was
    // parked on it when the read was taken - so the answer's own mark is what
    // holds the dock down until the stand-down for it lands.
    harness.page.record = record({ pending_asks: [slackDraftAsk()] });
    flushSync();
    expect(document.querySelector('.dock'), 'a repaint does not raise it again').toBeNull();

    harness.page.record = record({
      pending_asks: [slackDraftAsk({ id: '0192e1c0-0000-7000-8000-000000000001' })],
    });
    flushSync();
    expect(document.querySelector('.dock'), 'while the next draft is a new prompt').not.toBeNull();
  });

  it('says what became of a draft that left without this reader answering', () => {
    const harness = open({ record: record({ pending_asks: [slackDraftAsk()] }) });
    expect(document.querySelector('.dock'), 'the dock is up').not.toBeNull();

    // Another view answered it: the stand-down carries the ending, and the
    // record goes with it as the apply arm leaves it.
    harness.say({
      kind: 'update',
      update: {
        slack_draft_resolved: {
          key: SLOT,
          id: '0192e1c0-0000-7000-8000-000000000000',
          ending: { answered: { approved: true } },
        },
      },
    });
    harness.page.record = record();
    flushSync();

    expect(document.querySelector('.dock'), 'the dock stands down').toBeNull();
    expect(drawn(), 'and the row says which ending took it').toContain('posted from another view');
  });

  it('says why an answer did not land when the draft left under the click', () => {
    const harness = open({ record: record({ pending_asks: [slackDraftAsk()] }) });

    action('Post').click();
    flushSync();
    expect(document.querySelector('.dock'), 'the dock stands down for the click').toBeNull();

    // The core resolved it elsewhere, so the click's answer is refused: the
    // dock is already gone, and the refusal names its own operation, so the
    // reason is drawn where it stood.
    harness.say({
      kind: 'error',
      what: 'respond_slack_post',
      why: 'that Slack draft is no longer waiting: it has been answered, or its asking session went away',
    });
    flushSync();

    expect(drawn(), 'the refusal lands where the dock stood').toContain('no longer waiting');
  });

  it("says nothing when the reader's own answer is the one that took the draft", () => {
    const harness = open({ record: record({ pending_asks: [slackDraftAsk()] }) });

    action('Post').click();
    flushSync();

    harness.say({
      kind: 'update',
      update: {
        slack_draft_resolved: {
          key: SLOT,
          id: '0192e1c0-0000-7000-8000-000000000000',
          ending: { answered: { approved: true } },
        },
      },
    });
    harness.page.record = record();
    flushSync();

    expect(document.querySelector('.dock'), 'the dock is gone with the answer').toBeNull();
    expect(drawn(), 'and nothing is said about a draft this reader answered').not.toContain(
      'another view',
    );
  });

  it('does not read a later refusal as the draft it answered', () => {
    const harness = open({ record: record({ pending_asks: [slackDraftAsk()] }) });

    action('Post').click();
    flushSync();
    harness.say({
      kind: 'update',
      update: {
        slack_draft_resolved: {
          key: SLOT,
          id: '0192e1c0-0000-7000-8000-000000000000',
          ending: { answered: { approved: true } },
        },
      },
    });
    harness.page.record = record();
    flushSync();

    // The reader's next send is refused, and that refusal belongs to the
    // send: reading it as the answered draft would leave the send sending
    // forever and put its reason in a row nothing asked for.
    type('hello');
    sendBox();
    harness.say({ kind: 'error', what: 'dispatch', why: 'the connector is not configured' });
    flushSync();

    expect(echoAt(SLOT)?.state, 'the send is the row that failed').toBe('failed');
    expect(drawn(), 'and the draft row says nothing of it').not.toContain('another view');
  });

  it('keeps the ending for a seat the reader has left', () => {
    const harness = open({ record: record({ pending_asks: [slackDraftAsk()] }) });
    harness.page.slot = ELSEWHERE;
    harness.page.record = record({ slot: ELSEWHERE });
    flushSync();

    harness.say({
      kind: 'update',
      update: {
        slack_draft_resolved: {
          key: SLOT,
          id: '0192e1c0-0000-7000-8000-000000000000',
          ending: 'abandoned',
        },
      },
    });
    flushSync();
    expect(drawn(), 'nothing is drawn on the seat showing now').not.toContain(
      'asking session went away',
    );

    harness.page.slot = SLOT;
    harness.page.record = record();
    flushSync();

    expect(drawn(), 'and the seat it happened to meets the line on return').toContain(
      'asking session went away',
    );
  });

  it('draws the next held draft as its own body, not the one before it', () => {
    const harness = open({ record: record({ pending_asks: [slackDraftAsk()] }) });

    expect(drawn(), 'the first draft').toContain('Deploy finished on staging.');

    // The second draft, which is the next prompt the seat is parked on - with a
    // body of its own, which is what tells a fresh dock from a carried-over one.
    harness.page.record = record({
      pending_asks: [
        slackDraftAsk({
          id: '0192e1c0-0000-7000-8000-000000000001',
          text: 'The rollback finished; production is on the previous build.',
        }),
      ],
    });
    flushSync();

    expect(drawn(), 'the next draft draws its own body').toContain(
      'The rollback finished; production is on the previous build.',
    );
    expect(drawn(), 'and the one before it is gone').not.toContain('Deploy finished on staging.');
  });

  it('refuses the held draft from its own no-verb', () => {
    const harness = open({ record: record({ pending_asks: [slackDraftAsk()] }) });

    action("Don't send").click();
    flushSync();

    expect(commands(harness)).toEqual([
      {
        respond_slack_post: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          id: '0192e1c0-0000-7000-8000-000000000000',
          approved: false,
        },
      },
    ]);
  });

  it('refuses the held draft on Escape, which is the same refusal', () => {
    const harness = open({ record: record({ pending_asks: [slackDraftAsk()] }) });

    press('Escape');

    expect(commands(harness)).toEqual([
      {
        respond_slack_post: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          id: '0192e1c0-0000-7000-8000-000000000000',
          approved: false,
        },
      },
    ]);
  });

  it('draws the hand-off as the session asked it, with the verbs that reach its answer', () => {
    open({ record: record({ pending_asks: [browserHandOffAsk()] }) });

    expect(drawn()).toContain('needs you');
    expect(drawn()).toContain('browser hand-off · tab 2');
    expect(drawn(), "the reason is the session's own words, in full").toContain(
      'The sign-in page is showing a CAPTCHA.',
    );
    expect(drawn(), "Open is the client's own door").toContain('Open browser');
    expect(drawn(), 'and both answers are reachable').toContain('Done');
    expect(drawn()).toContain('Not now');
  });

  it('says why an answer did not land when the hand-off left under the click', async () => {
    const harness = open({ record: record({ pending_asks: [browserHandOffAsk()] }) });

    action('Open browser').click();
    await raised();
    barAction('Done').click();
    flushSync();
    expect(document.querySelector('.dock'), 'the dock stands down for the click').toBeNull();

    // The core resolved it elsewhere, so the click's answer is refused: the
    // reason rides the hand-off's own operation name and is drawn where the
    // dock stood.
    harness.say({
      kind: 'error',
      what: 'respond_browser_hand_off',
      why: 'that browser hand-off is no longer waiting: it has been answered, or its asking session went away',
    });
    flushSync();

    expect(drawn(), 'the reason is drawn where the dock stood').toContain('no longer waiting');
  });

  /** **The dock card's own door before the takeover is up.** The page writes
   *  this word by hand and nothing else ties it to the core's enum, so a
   *  rename here would compile, keep every suite green, and leave every
   *  answer given before Open undecodable - the exact failure the wire test
   *  exists for, and the one door no client test used to cover. */
  it('answers the dock card with its own door before Open', () => {
    const harness = open({ record: record({ pending_asks: [browserHandOffAsk()] }) });

    action('Done').click();
    flushSync();

    expect(commands(harness)).toEqual([
      {
        respond_browser_hand_off: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          id: '0192e1c0-0000-7000-8000-0000000000aa',
          answer: { ending: { type: 'done' }, source: 'dock' },
        },
      },
    ]);
  });

  it('answers the hand-off as the verb says: Done settles it', async () => {
    const harness = open({ record: record({ pending_asks: [browserHandOffAsk()] }) });

    action('Open browser').click();
    await raised();
    barAction('Done').click();
    flushSync();

    expect(commands(harness)).toEqual([
      {
        respond_browser_hand_off: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          id: '0192e1c0-0000-7000-8000-0000000000aa',
          answer: { ending: { type: 'done' }, source: 'takeover' },
        },
      },
    ]);
    // **Answering also lowers the window** (Ved live, 2026-10-07: "I said
    // done, but the browser tab is still open"): Open raised it, and the
    // answer is what takes it down. This line was the live bug's fix and
    // nothing pinned it.
    expect(hideBrowser, 'the window comes down with the answer').toHaveBeenCalledTimes(1);
    // **The attach this page holds is the one that comes down**: Done lowers
    // the tab Open raised, never another surface's.
    expect(hideBrowser, 'and it is that raise that comes down').toHaveBeenCalledWith(null, SLOT);
  });

  /** **A hand-off that named no profile still lowers the browser its Open
   *  raised.** The raise resolves it to the asking seat's own profile; Done
   *  passes that seat, so the shell resolves the lower the same way. Without
   *  the seat the page asked for the shared browser, the seat's surface
   *  stayed up, and Done left the takeover covering the chat. */
  it('lowers the seat-own browser of a hand-off with no readable tab', async () => {
    vi.mocked(showBrowser).mockResolvedValueOnce({ profile: 'forge-lead', tab: 1 });
    open({ record: record({ pending_asks: [browserHandOffAsk({ tab: null })] }) });

    action('Open browser').click();
    await vi.waitFor(() =>
      expect(showBrowser, 'the raise names no tab and carries the seat').toHaveBeenCalledWith(
        null,
        SLOT,
        null,
      ),
    );
    await raised();
    barAction('Done').click();
    flushSync();

    expect(hideBrowser, 'Done carries the seat for the raise it is lowering').toHaveBeenCalledWith(
      null,
      SLOT,
    );
  });

  /**
   * **A surface raised for another seat is not this seat's, and does not stay
   * over it.** The page covers the chat, so the hand-off's card - and its
   * reason - was behind it: the person saw a browser they did not open over
   * the seat they moved to, and no ask. Leaving the seat that raised a page
   * lowers it, which puts the chat and this seat's card back.
   */
  it("lowers a takeover raised for another seat, so this seat's ask is on screen", async () => {
    await surfaceOn(ELSEWHERE, 2);
    open({ record: record({ pending_asks: [browserHandOffAsk()] }) });

    await vi.waitFor(() =>
      expect(surface.phase, "the other seat's page comes down").toBe('closed'),
    );
    flushSync();

    expect(hideBrowser, "and the lower is the shell's").toHaveBeenCalled();
    expect(document.querySelector('.takeover-bar'), 'no bar stands over this seat').toBeNull();
    expect(drawn(), 'the ask is drawn where the person is looking').toContain(
      'The sign-in page is showing a CAPTCHA.',
    );
  });

  /** **A switch is not a mount.** The page hands this composer one seat's
   *  record after another and keeps it mounted, so the lower must follow the
   *  `slot` prop: the incident's path was a seat change, not a page load. */
  it("lowers this seat's page when the reader moves to another seat", async () => {
    await surfaceOn(SLOT, 2);
    const harness = open({ record: record({ pending_asks: [browserHandOffAsk()] }) });
    expect(surface.phase, 'precondition: this seat opened its page').toBe('live');

    harness.page.slot = ELSEWHERE;
    harness.page.record = record({ slot: ELSEWHERE, pending_asks: [browserHandOffAsk()] });
    flushSync();

    await vi.waitFor(() =>
      expect(surface.phase, 'the page the seat left behind comes down').toBe('closed'),
    );
    flushSync();
    expect(
      document.querySelector('.takeover-bar'),
      'and no bar stands over the seat moved to',
    ).toBeNull();
    expect(drawn(), 'the seat moved to draws its own ask').toContain(
      'The sign-in page is showing a CAPTCHA.',
    );
  });

  /** **A lower that cannot land leaves the other seat's page over this one**,
   *  failed and still owned by the seat that raised it. The bar must not
   *  answer this seat's ask from it: the seat check is what holds when the
   *  page cannot be taken down. */
  it("answers nothing from another seat's failed page", async () => {
    await surfaceOn(ELSEWHERE, 2);
    vi.mocked(hideBrowser).mockResolvedValue('the shell is not answering');
    open({ record: record({ pending_asks: [browserHandOffAsk()] }) });

    await vi.waitFor(() => expect(surface.phase, 'the lower was refused').toBe('failed'));
    flushSync();

    expect(surface.seat, "the page is still the other seat's").toEqual(ELSEWHERE);
    expect(barButtons(), "so this seat's ask is not answered from it").not.toContain('Done');
    expect(barText(), 'and its words do not ride it').not.toContain(
      'The sign-in page is showing a CAPTCHA.',
    );
  });

  /** **The press is not the page.** While the attach is in flight the ask
   *  already rides the bar, but nothing may answer it until the page the ask
   *  named is actually up. */
  it('offers the ask while the page opens, and no answers until it is up', async () => {
    let release: ((answer: { profile: string; tab: number }) => void) | null = null;
    vi.mocked(showBrowser).mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          release = resolve;
        }),
    );
    open({ record: record({ pending_asks: [browserHandOffAsk()] }) });

    action('Open browser').click();
    flushSync();

    expect(surface.phase, 'the page is on its way').toBe('opening');
    expect(barText(), 'the ask rides the bar from the press').toContain(
      'The sign-in page is showing a CAPTCHA.',
    );
    expect(barButtons(), 'but its answers wait for the page').not.toContain('Done');

    (release as ((answer: { profile: string; tab: number }) => void) | null)?.({
      profile: 'job-hunt',
      tab: 2,
    });
    await raised();
    expect(barButtons(), 'and arrive with it').toContain('Done');
  });

  /**
   * **The tab is half of the ask.** A surface showing another tab is a page
   * the session did not prepare, so its Done and feedback line are not this
   * ask's answers - but the ask itself must still be named, with the tab it
   * wants, because the card carrying it is behind the page.
   */
  it('names the ask on a surface showing another tab, and offers no answer', async () => {
    await surfaceOn(SLOT, 3);
    open({ record: record({ pending_asks: [browserHandOffAsk()] }) });

    expect(barButtons(), 'the wrong tab does not answer').not.toContain('Done');
    expect(barButtons(), 'and the feedback line is not offered for it').not.toContain(
      "something's wrong…",
    );
    expect(barText(), 'and the ask is still named, with the tab it wants').toContain(
      'browser hand-off · tab 2',
    );
    expect(barText(), "with the session's own words").toContain(
      'The sign-in page is showing a CAPTCHA.',
    );
  });

  /**
   * **The bar carries the ask.** With the page covering the chat, the reason
   * exists only on the card behind it; the bar must say what is asked wherever
   * its answers are on screen.
   */
  it("carries the ask's words on the surface it stands for", async () => {
    await surfaceOn(SLOT, 2);
    open({ record: record({ pending_asks: [browserHandOffAsk()] }) });

    expect(barText(), "the session's own words, on the bar that answers").toContain(
      'The sign-in page is showing a CAPTCHA.',
    );
    expect(barButtons(), 'with the answers beside them').toContain('Done');
  });

  /** **A page that failed is still the ask's page.** The host dying under a
   *  parked hand-off is the one case the person most needs to report - "the
   *  browser exited" is a message to the session, not a decline - so the ask
   *  and the feedback line stay on the bar. */
  it('keeps the ask and its answers when the page fails under it', async () => {
    open({ record: record({ pending_asks: [browserHandOffAsk()] }) });
    vi.mocked(showBrowser).mockResolvedValueOnce('the browser host exited (signal 11)');

    action('Open browser').click();
    await vi.waitFor(() => expect(surface.phase, 'the page failed').toBe('failed'));
    flushSync();

    expect(barText(), 'the ask stays on the bar').toContain(
      'The sign-in page is showing a CAPTCHA.',
    );
    expect(barButtons(), 'and the person can still answer it').toContain('Done');
    expect(barButtons(), 'and can say the host died').toContain("something's wrong…");
  });

  /**
   * **A take begun at the bar belongs to the bar.** Its mic is the only door
   * a take has while the page owns the keyboard, and the words it brings are
   * the feedback line's - not the chat draft's, which is hidden behind the
   * page.
   */
  it("lands a take's words in the feedback line the bar stands for", async () => {
    await surfaceOn(SLOT, 2);
    const harness = open({
      dictation: true,
      record: record({ pending_asks: [browserHandOffAsk()] }),
    });
    barAction('something').click();
    flushSync();

    await beginBarTake(harness);
    harness.page.record = record({
      pending_asks: [browserHandOffAsk()],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'the words', truncated: false },
        machine: null,
      },
    });
    flushSync();

    const field = document.querySelector<HTMLTextAreaElement>('.takeover-bar textarea');
    expect(field, 'the bar keeps its feedback line').not.toBeNull();
    expect(field?.value, "and the take's words land in it").toBe('the words');
  });

  /** **An answer from another view lowers the window here.** The local Done
   *  takes its own window down; a Done from another view never touches this
   *  shell, and without this the browser stays headed for the next agent call
   *  to attach to while the answer says the person finished. */
  it('draws the document hand-off with its path and both verbs', () => {
    open({ record: record({ pending_asks: [documentHandOffAsk()] }) });

    expect(drawn()).toContain('needs you');
    expect(drawn()).toContain('document hand-off');
    expect(drawn(), "the document's own address, in full").toContain('/proj/docs/plan.md');
    expect(drawn(), 'Open and the decline are both reachable').toContain('Open');
    expect(drawn()).toContain('Not now');
  });

  it('answers the document hand-off with its decline, which is the release', () => {
    const harness = open({ record: record({ pending_asks: [documentHandOffAsk()] }) });

    action('Not now').click();
    flushSync();

    expect(commands(harness)).toEqual([
      {
        answer_document_hand_off: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          id: '0192e1c0-0000-7000-8000-0000000000dd',
          answer: 'not_now',
        },
      },
    ]);
  });

  /** **Open answers nothing.** The parked call releases on the viewer's own
   *  done, so opening is the client's route and the session hears only what
   *  the reading produced. */
  it('opens the document without answering the parked call', () => {
    const harness = open({ record: record({ pending_asks: [documentHandOffAsk()] }) });

    action('Open').click();
    flushSync();

    expect(commands(harness), 'opening is not the answer').toEqual([]);
    expect(location.pathname, 'the route is the document, under its seat').toContain('/doc/');
  });

  /** **An answer from another view lowers the window here.** The local Done
   *  takes its own window down; a Done from another view never touches this
   *  shell, and without this the browser stays headed for the next agent call
   *  to attach to while the answer says the person finished. */
  it('lowers the window when another view answers the hand-off', () => {
    const harness = open({ record: record({ pending_asks: [browserHandOffAsk()] }) });
    expect(hideBrowser, 'nothing is lowered while the hand-off waits').not.toHaveBeenCalled();

    harness.say({
      kind: 'update',
      update: {
        browser_hand_off_resolved: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          id: '0192e1c0-0000-7000-8000-0000000000aa',
          answer: { ending: { type: 'done' }, source: 'takeover', client: 1 },
        },
      },
    });

    expect(hideBrowser, 'the attach this page holds comes down').toHaveBeenCalledWith(null, SLOT);
  });

  it('declines the hand-off with Not now, which is the same release', async () => {
    const harness = open({ record: record({ pending_asks: [browserHandOffAsk()] }) });

    // Not now is the takeover bar's; Open puts the surface up.
    action('Open browser').click();
    await raised();
    barAction('Not now').click();
    flushSync();

    expect(commands(harness)).toEqual([
      {
        respond_browser_hand_off: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          id: '0192e1c0-0000-7000-8000-0000000000aa',
          answer: { ending: { type: 'not_now' }, source: 'takeover' },
        },
      },
    ]);
    // The answer lowers the page it was given on, which is also the door a
    // shell that refuses every lower would fail to close.
    await vi.waitFor(() => expect(surface.phase, 'the answer lowers the page').toBe('closed'));
  });

  it('offers a retry when the surface failed, without leaving the hand-off', async () => {
    open({ record: record({ pending_asks: [browserHandOffAsk()] }) });
    vi.mocked(showBrowser).mockResolvedValueOnce('the browser did not start in time - try again');

    action('Open browser').click();
    await Promise.resolve();
    await Promise.resolve();
    flushSync();
    expect(surface.phase, 'the surface failed').toBe('failed');
    expect(surface.reason).toContain('did not start in time');
    expect(drawn(), 'and the bar offers the door back').toContain('try again');

    vi.mocked(showBrowser).mockResolvedValueOnce({ profile: 'job-hunt', tab: 2 });
    barAction('try again').click();
    await Promise.resolve();
    await Promise.resolve();
    flushSync();

    expect(showBrowser).toHaveBeenCalledTimes(2);
    expect(
      showBrowser,
      'the retry asks for the profile current tab, not the parked ask dead tab',
    ).toHaveBeenLastCalledWith(null, SLOT, null);
    expect(surface.phase, 'the second ask landed').toBe('live');
  });

  /** **A person reporting a problem is not a person declining.** The feedback
   *  line sends their words as a prompt and then ends the park, and the ending
   *  it sends must say they spoke - `not_now` here told the session "the person
   *  declined, decide what to do instead" while they were still working the
   *  page (live, 2026-10-26: two parks ended that way with the person's own
   *  words describing the page). */
  it('sends the bar feedback to the agent and ends the park as spoken, not declined', async () => {
    const harness = open({ record: record({ pending_asks: [browserHandOffAsk()] }) });

    action('Open browser').click();
    await raised();
    barAction('something').click();
    flushSync();

    const field = document.querySelector<HTMLTextAreaElement>('.takeover-bar textarea');
    if (!(field instanceof HTMLTextAreaElement)) throw new Error('the bar drew no feedback line');
    field.value = 'the submit button did nothing';
    field.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();

    const send = document.querySelector('.takeover-bar .sendb');
    if (!(send instanceof HTMLElement)) throw new Error('the bar drew no send');
    send.click();
    // The ending answers only once the connection says the prompt crossed.
    await vi.waitFor(() => expect(commands(harness)).toHaveLength(2));
    flushSync();

    const [prompt, answer] = commands(harness);
    expect((prompt?.['prompt_under'] as Record<string, unknown>)['text']).toBe(
      'the submit button did nothing',
    );
    expect(answer).toEqual({
      respond_browser_hand_off: {
        key: { org: 'Busytools', project: 'forge', label: 'lead' },
        id: '0192e1c0-0000-7000-8000-0000000000aa',
        answer: { ending: { type: 'feedback' }, source: 'feedback' },
      },
    });
  });

  /** **Words that never left are not a message to read.** A send the socket
   *  refused used to answer `feedback` anyway, telling the session to read a
   *  prompt nobody sent and ending the park on nothing; the dock stays live,
   *  so the reader can press send again when the socket is back. */
  it('keeps the hand-off parked when the feedback never left', async () => {
    const shared = wire();
    const harness = open({ record: record({ pending_asks: [browserHandOffAsk()] }) }, shared);

    action('Open browser').click();
    await raised();
    barAction('something').click();
    flushSync();
    const field = document.querySelector<HTMLTextAreaElement>('.takeover-bar textarea');
    if (!(field instanceof HTMLTextAreaElement)) throw new Error('the bar drew no feedback line');
    field.value = 'the submit button did nothing';
    field.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    const send = document.querySelector('.takeover-bar .sendb');
    if (!(send instanceof HTMLElement)) throw new Error('the bar drew no send');

    shared.down = true;
    send.click();
    flushSync();
    expect(commands(harness), 'the refused send answers nothing').toEqual([]);

    shared.down = false;
    send.click();
    await vi.waitFor(() =>
      expect(
        commands(harness).map((command) => Object.keys(command)[0]),
        'and the retry sends the words and then the ending',
      ).toEqual(['prompt_under', 'respond_browser_hand_off']),
    );
  });

  /**
   * **The window #2688 names**: the socket is gone but the page has not
   * processed the status, so the dispatch is taken and then refused rather
   * than throwing here. Answering `feedback` on that would tell the session
   * to read a prompt nobody sent; the bar keeps the words and the park.
   */
  it('keeps the words and the park when a send that looked open never crossed', async () => {
    const shared = wire();
    const harness = open({ record: record({ pending_asks: [browserHandOffAsk()] }) }, shared);

    action('Open browser').click();
    await raised();
    barAction('something').click();
    flushSync();
    const field = document.querySelector<HTMLTextAreaElement>('.takeover-bar textarea');
    if (!(field instanceof HTMLTextAreaElement)) throw new Error('the bar drew no feedback line');
    field.value = 'the submit button did nothing';
    field.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    const send = document.querySelector('.takeover-bar .sendb');
    if (!(send instanceof HTMLElement)) throw new Error('the bar drew no send');

    shared.dead = true;
    send.click();
    // The bar's own line is what the failure leaves, so once it is drawn the
    // catch has run and the assertions below are safe to make.
    await vi.waitFor(() => expect(drawn()).toContain('Not sent - the connection is down.'));
    flushSync();
    expect(commands(harness), 'the refused send answers nothing').toEqual([]);
    expect(surface.phase, 'the takeover stays up for the retry').toBe('live');
    expect(field.value, 'and the words stay where they were typed').toBe(
      'the submit button did nothing',
    );

    shared.dead = false;
    send.click();
    await vi.waitFor(() =>
      expect(
        commands(harness).map((command) => Object.keys(command)[0]),
        'the retry sends the words and then the ending',
      ).toEqual(['prompt_under', 'respond_browser_hand_off']),
    );
    const prompt = commands(harness)[0]?.['prompt_under'] as Record<string, unknown>;
    expect(prompt['text'], 'the retry carries the same words').toBe(
      'the submit button did nothing',
    );
  });

  /** **A key inside the raised takeover is the page's, not the dock's.** The
   *  native overlay holds the keyboard while the surface is up, and a key the
   *  bar itself receives must not reach the dock's refusal - Escape in a
   *  raised takeover declining the hand-off was the first suspect in the live
   *  incident, and this is the property that rules it out. */
  it('does not answer the hand-off on Escape inside the raised takeover', async () => {
    const harness = open({ record: record({ pending_asks: [browserHandOffAsk()] }) });

    action('Open browser').click();
    await raised();
    barAction('something').click();
    flushSync();

    const field = document.querySelector<HTMLTextAreaElement>('.takeover-bar textarea');
    if (!(field instanceof HTMLTextAreaElement)) throw new Error('the bar drew no feedback line');
    field.focus();
    press('Escape');

    expect(commands(harness), 'the hand-off is still parked, nothing answered it').toEqual([]);
  });

  /**
   * **A take begun at the bar stays the bar's while its takeover stands.**
   * A failed page keeps the feedback line (the person can say the host died),
   * and its retry puts the surface back into opening with the take still
   * recording. The words were spoken for that line, so they land there: the
   * composer's draft is behind the picture, where a press reads as dead.
   */
  it("keeps a bar take's words in the bar while a retry opens the page under it", async () => {
    const harness = await failedPageTaking();
    const { land } = retryHeld();
    expect(surface.phase, 'the retry is opening the page').toBe('opening');

    // The take ends while the page is opening, and its words come back.
    landBarTake(harness);
    expect(
      document.querySelector<HTMLTextAreaElement>('.takeover-bar textarea')?.value,
      'the words are on the line while the retry opens the page',
    ).toBe('the words');
    expect(
      document.querySelector('.takeover-bar .sendb'),
      'a line that cannot answer offers no send',
    ).toBeNull();

    // The retry asked for tab null; the shell answers with the ask's own tab,
    // so the page is answerable again and the line stays it.
    land({ profile: 'job-hunt', tab: 2 });
    await vi.waitFor(() => expect(surface.phase, 'the retried page is up').toBe('live'));
    flushSync();
    const line = document.querySelector<HTMLTextAreaElement>('.takeover-bar textarea');
    if (!(line instanceof HTMLTextAreaElement)) throw new Error('the bar drew no feedback line');
    expect(line.value, 'the words were held for the line the reader spoke them into').toBe(
      'the words',
    );
    expect(
      document.querySelector('.takeover-bar .sendb'),
      'the page answers again, so the send is back',
    ).not.toBeNull();

    // The ask resolves: the line goes with it, so the words land in the
    // composer once - and no second copy stays behind.
    harness.page.record = record({ pending_asks: [] });
    flushSync();
    expect(field().value, 'the words land in the composer, once').toBe('the words');
  });

  /** A page that fails its first open, with the feedback line open and a take
   *  running at the bar's mic, which is where every carry case starts. */
  async function failedPageTaking(): Promise<ReturnType<typeof open>> {
    vi.mocked(showBrowser).mockResolvedValueOnce('the browser host exited');
    const harness = open({
      dictation: true,
      record: record({ pending_asks: [browserHandOffAsk()] }),
    });
    action('Open browser').click();
    await vi.waitFor(() => expect(surface.phase, 'the page failed').toBe('failed'));
    flushSync();
    barAction('something').click();
    flushSync();
    await startBarTake();
    return harness;
  }

  /**
   * **A failed page keeps the line, and the line keeps the words.** After the
   * first failed open the surface holds no tab list, so the retry asks for
   * none and the ask still names tab 2. A line that cannot answer still shows
   * what was spoken into it - showing the words is not answering - and offers
   * no send it could not honour.
   */
  it("keeps a bar take's words on the line when the retry fails again", async () => {
    const harness = await failedPageTaking();
    const { land } = retryHeld();
    landBarTake(harness);

    land('the browser host exited again');
    await vi.waitFor(() => expect(surface.phase, 'the retry failed too').toBe('failed'));
    flushSync();

    expect(
      document.querySelector<HTMLTextAreaElement>('.takeover-bar textarea')?.value,
      'the words are still on the line',
    ).toBe('the words');
    expect(
      document.querySelector('.takeover-bar .sendb'),
      'and the line offers no send it cannot honour',
    ).toBeNull();
  });

  /** The same when the retry lands on a page the ask did not name. */
  it("keeps a bar take's words on the line when the retry lands elsewhere", async () => {
    const harness = await failedPageTaking();
    const { land } = retryHeld();
    landBarTake(harness);

    land({ profile: 'job-hunt', tab: 3 });
    await vi.waitFor(() => expect(surface.phase, 'the retried page is up').toBe('live'));
    flushSync();

    expect(
      document.querySelector<HTMLTextAreaElement>('.takeover-bar textarea')?.value,
      'the words are still on the line',
    ).toBe('the words');
    expect(
      document.querySelector('.takeover-bar .sendb'),
      'and the line offers no send it cannot honour',
    ).toBeNull();
  });

  /** **The line is the ask's, not the phase's.** Another view answering the
   *  ask mid-take must not leave the words in a line nothing draws. */
  it("carries a bar take's words when another view answers the ask mid-take", async () => {
    const harness = await failedPageTaking();
    const { land } = retryHeld();

    // The other view's answer lands as the record dropping the ask.
    harness.page.record = record({ pending_asks: [] });
    flushSync();
    landBarTake(harness, []);
    land({ profile: 'job-hunt', tab: 2 });
    await vi.waitFor(() => expect(surface.phase, 'the retried page is up').toBe('live'));
    flushSync();

    expect(field().value, 'the words go to the composer, not a line nothing draws').toBe(
      'the words',
    );
  });

  /** **The line is the ask's, whatever tab is showing.** A reader on another
   *  tab when the take lands still gets the words on the line, with no send. */
  it("keeps a bar take's words on the line when the reader is on another tab as they land", async () => {
    await surfaceOn(SLOT, 2);
    const harness = open({
      dictation: true,
      record: record({ pending_asks: [browserHandOffAsk()] }),
    });
    barAction('something').click();
    flushSync();
    await startBarTake();

    vi.mocked(showBrowser).mockResolvedValueOnce({ profile: 'job-hunt', tab: 3 });
    await selectTab(3);
    flushSync();
    expect(surface.tab, 'the reader left the ask its tab').toBe(3);

    landBarTake(harness);
    expect(
      document.querySelector<HTMLTextAreaElement>('.takeover-bar textarea')?.value,
      'the words are on the line',
    ).toBe('the words');
    expect(
      document.querySelector('.takeover-bar .sendb'),
      'and the line offers no send it cannot honour',
    ).toBeNull();
  });

  /**
   * **A lowered takeover hands its words to the chat, and the card says so.**
   * The line goes with the page, so the carry joins the words into the
   * composer's draft - and the composer is behind the hand-off's card until
   * the ask is answered, so the card is where the person reads what happened.
   * The line must not redraw them and send them a second time.
   */
  it("hands a lowered takeover's words to the chat, and the card says so", async () => {
    const harness = await failedPageTaking();
    landBarTake(harness);
    barAction('back to chat').click();
    await vi.waitFor(() => expect(surface.phase, 'the chat is back').toBe('closed'));
    flushSync();
    expect(drawn(), 'the card says where the words went').toContain(
      "What you said at the bar is in the chat's draft: the words",
    );

    // The line cannot redraw and re-send the same words.
    action('Open browser').click();
    await vi.waitFor(() => expect(surface.phase, 'the page is up again').toBe('live'));
    flushSync();
    barAction('something').click();
    flushSync();
    expect(
      document.querySelector<HTMLTextAreaElement>('.takeover-bar textarea')?.value,
      'the line is empty: the words went to the chat',
    ).toBe('');

    // Answering settles it: the composer shows what the card said it held.
    barAction('Done').click();
    await vi.waitFor(() => expect(surface.phase, 'the answer lowered it').toBe('closed'));
    harness.page.record = record({ pending_asks: [] });
    flushSync();
    expect(field().value, 'the words are in the chat, visible').toBe('the words');
  });

  /** **An ask answered from elsewhere still hands the line's words over.**
   *  The record can drop the ask before the surface is lowered, so the words
   *  have to be in the seat's draft by then - and the composer draws them
   *  once the card is gone. */
  it("keeps a bar take's words when another view answers the ask", async () => {
    const harness = await failedPageTaking();
    landBarTake(harness);
    expect(
      document.querySelector<HTMLTextAreaElement>('.takeover-bar textarea')?.value,
      'the words are on the line',
    ).toBe('the words');

    // Another view answered: the record drops the ask first.
    harness.page.record = record({ pending_asks: [] });
    flushSync();

    expect(field().value, 'the words are in the composer once the card is gone').toBe('the words');
  });

  /** **An emptied line hands nothing over.** The words land, the reader
   *  clears them, then lowers the takeover: the card must not claim words
   *  that are not there, and a draft already in the chat gains no stray
   *  space. */
  it('hands nothing over when the reader emptied the line', async () => {
    vi.mocked(showBrowser).mockResolvedValueOnce('the browser host exited');
    const harness = open({ dictation: true });
    type('hello');
    harness.page.record = record({ pending_asks: [browserHandOffAsk()] });
    flushSync();
    action('Open browser').click();
    await vi.waitFor(() => expect(surface.phase, 'the page failed').toBe('failed'));
    flushSync();
    barAction('something').click();
    flushSync();
    await startBarTake();
    landBarTake(harness);

    const line = document.querySelector<HTMLTextAreaElement>('.takeover-bar textarea');
    if (!(line instanceof HTMLTextAreaElement)) throw new Error('the bar drew no feedback line');
    line.value = '   ';
    line.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();

    barAction('back to chat').click();
    await vi.waitFor(() => expect(surface.phase, 'the chat is back').toBe('closed'));
    flushSync();
    expect(drawn(), 'the card claims no words').not.toContain('What you said at the bar');

    harness.page.record = record({ pending_asks: [] });
    flushSync();
    expect(field().value, 'the draft is the one the reader had, untouched').toBe('hello');
  });

  /** **The carry joins, it does not clobber.** A draft the reader already had
   *  keeps its words when the line's come in behind them. */
  it('joins a carried line to the draft the reader already had', async () => {
    vi.mocked(showBrowser).mockResolvedValueOnce('the browser host exited');
    const harness = open({ dictation: true });
    type('hello');
    harness.page.record = record({ pending_asks: [browserHandOffAsk()] });
    flushSync();
    action('Open browser').click();
    await vi.waitFor(() => expect(surface.phase, 'the page failed').toBe('failed'));
    flushSync();
    barAction('something').click();
    flushSync();
    await startBarTake();
    landBarTake(harness);
    barAction('back to chat').click();
    await vi.waitFor(() => expect(surface.phase, 'the chat is back').toBe('closed'));
    flushSync();

    harness.page.record = record({ pending_asks: [] });
    flushSync();
    expect(field().value, "the reader's own words come first").toBe('hello the words');
  });

  /** **A parked hand-off's words are the seat's.** A visit to another seat
   *  must not release them; coming back, they are still there to read. */
  it("keeps a parked hand-off's words across a visit to another seat", async () => {
    const harness = await failedPageTaking();
    landBarTake(harness);

    harness.page.slot = ELSEWHERE;
    flushSync();
    harness.page.record = record({ slot: ELSEWHERE });
    flushSync();
    // The page shows this seat again before its record arrives, which is the
    // frame the guard is for: the record in hand is still the other seat's.
    harness.page.slot = SLOT;
    flushSync();
    harness.page.record = record({ pending_asks: [browserHandOffAsk()] });
    flushSync();

    expect(
      document.querySelector<HTMLTextAreaElement>('.takeover-bar textarea')?.value,
      "the words are still on the seat's line",
    ).toBe('the words');
  });

  it('declines the hand-off on Escape, which the dock row promises', () => {
    const harness = open({ record: record({ pending_asks: [browserHandOffAsk()] }) });

    press('Escape');
    flushSync();

    expect(
      commands(harness),
      'Escape is the Not now door: without it the key would do nothing and the parked call - ' +
        'which has no timeout - would wait forever',
    ).toEqual([
      {
        respond_browser_hand_off: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          id: '0192e1c0-0000-7000-8000-0000000000aa',
          answer: { ending: { type: 'not_now' }, source: 'dock_escape' },
        },
      },
    ]);
  });

  it('says what became of a hand-off that left without this reader answering', () => {
    const harness = open({ record: record({ pending_asks: [browserHandOffAsk()] }) });
    expect(document.querySelector('.dock'), 'the dock is up').not.toBeNull();

    // Another view answered it: the stand-down carries the ending, and the
    // record goes with it as the apply arm leaves it.
    harness.say({
      kind: 'update',
      update: {
        browser_hand_off_resolved: {
          key: SLOT,
          id: '0192e1c0-0000-7000-8000-0000000000aa',
          answer: { ending: { type: 'done' }, source: 'takeover', client: 1 },
        },
      },
    });
    harness.page.record = record();
    flushSync();

    expect(document.querySelector('.dock'), 'the dock stands down').toBeNull();
    expect(drawn(), 'and the row says which ending took it').toContain('settled in another view');
  });

  /** The feedback ending is a new word on the wire, and a client that does
   *  not know it falls through to "no longer waiting" - which would tell a
   *  reader nothing about a hand-off the person settled with a message. */
  it('says a hand-off another view settled with a message', () => {
    const harness = open({ record: record({ pending_asks: [browserHandOffAsk()] }) });

    harness.say({
      kind: 'update',
      update: {
        browser_hand_off_resolved: {
          key: SLOT,
          id: '0192e1c0-0000-7000-8000-0000000000aa',
          answer: { ending: { type: 'feedback' }, source: 'feedback', client: 1 },
        },
      },
    });
    harness.page.record = record();
    flushSync();

    expect(drawn(), 'the news is the message, not a silence').toContain(
      'settled with a message in another view',
    );
  });

  it("does not narrate the reader's own answer to a hand-off", async () => {
    const harness = open({ record: record({ pending_asks: [browserHandOffAsk()] }) });

    action('Open browser').click();
    await raised();
    barAction('Done').click();
    flushSync();
    expect(document.querySelector('.dock'), 'the dock stands down for the click').toBeNull();

    // The stand-down for THIS answer lands: the record drops the ask, and the
    // ending must not be read as another view's - that sentence would be
    // false text about the reader's own act.
    harness.say({
      kind: 'update',
      update: {
        browser_hand_off_resolved: {
          key: SLOT,
          id: '0192e1c0-0000-7000-8000-0000000000aa',
          answer: { ending: { type: 'done' }, source: 'takeover', client: 1 },
        },
      },
    });
    harness.page.record = record();
    flushSync();

    expect(drawn(), "the reader's own click is not another view's").not.toContain('another view');
  });

  /** **The window, not an overlay** (Ved, 2026-10-07): Open raises the
   * person's own browser over the profile the agents drive - and the raise
   * carries the ASKING SEAT, which the shell checks against the profile's
   * record before it raises anything. */
  it('opens the browser window', async () => {
    vi.mocked(showBrowser).mockResolvedValueOnce({ profile: 'job-hunt', tab: 2 });
    open({ record: record({ pending_asks: [browserHandOffAsk()] }) });

    action('Open browser').click();
    await vi.waitFor(() =>
      expect(showBrowser, 'the raise the approval is for').toHaveBeenCalledWith(null, SLOT, 2),
    );
  });

  /** A window that will not open says why, in the shell's own words: the
   *  sentence a machine with no browser to drive carries names what to
   *  install, and a dock that replaced it with boilerplate would throw the
   *  one actionable line away. */
  it("says the raise failed in the shell's own words", async () => {
    const harness = open({ record: record({ pending_asks: [browserHandOffAsk()] }) });
    vi.mocked(showBrowser).mockResolvedValueOnce(
      'the client is missing its Chromium framework - reinstall forge',
    );

    action('Open browser').click();
    await vi.waitFor(() => expect(drawn()).toContain('reinstall forge'));

    expect(commands(harness), 'and no answer crossed: Open answers nothing').toEqual([]);
  });

  it("draws the question's own mark for its header, not a character-cell glyph", () => {
    open({ record: record({ pending_asks: [questionAsk()] }) });

    expect(document.querySelector('.dock .qm use')?.getAttribute('href')).toBe('#i-question');
    expect(drawn(), 'and the queue line carries no glyph').not.toContain('▼');
  });

  /**
   * A focused action keeps the keys that activate it.
   *
   * A button's activation is native - jsdom does not run it, and a real engine
   * does - so what a test can pin is the half that is this component's: the
   * dock must not preventDefault the key on its way to the button, which is how
   * a permission lost every keyboard path to its actions.
   */
  it('does not swallow the keys a focused action answers to', () => {
    open({ record: record({ pending_asks: [permissionAsk()] }) });

    const allow = action('Allow once');
    allow.focus();
    flushSync();

    for (const key of ['Enter', ' ']) {
      const event = new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true });
      allow.dispatchEvent(event);
      flushSync();

      expect(event.defaultPrevented, `the dock swallowed ${key} on its way to the button`).toBe(
        false,
      );
    }
  });

  it('sends one answer, however many keys arrive after it', () => {
    const harness = open({
      record: record({ pending_asks: [questionAsk('tu-q', { multi_select: false })] }),
    });

    options()[1]?.click();
    flushSync();
    expect(commands(harness), 'the answer went').toHaveLength(1);

    press('Enter');
    press('Escape');
    flushSync();

    expect(commands(harness), 'and neither key sends a second one').toHaveLength(1);
  });

  /**
   * A refused take is not the answer's refusal.
   *
   * The wire's `dispatch` refusal cannot say which command it was about, and
   * taking it as the outstanding answer's own reason clears the stand-down -
   * which is the door the answer goes out of a second time. The door is tested
   * by a ROW CLICK rather than by Enter: the take's press puts the caret in the
   * words row, where Enter carries no words and answers nothing under either
   * shape, so a key there cannot tell a dock that has re-armed from one that
   * has not.
   *
   * The other direction is accepted rather than fixed: an answer's own refusal
   * arriving while a take is live draws on the take's line and does not clear
   * the stand-down, and the prompt clears both when it resolves.
   */
  it("does not take a refused take for the answer's own refusal", async () => {
    const harness = open({
      record: record({ pending_asks: [questionAsk('tu-q', { multi_select: false })] }),
      dictation: true,
    });

    /** The answers alone: the take's own commands are not what this counts. */
    const answers = () => commands(harness).filter((command) => 'respond_question' in command);

    options()[1]?.click();
    flushSync();
    expect(answers(), 'the answer went').toHaveLength(1);

    await micPress();
    harness.say({ kind: 'error', what: 'dispatch', why: 'dictation is not ready' });
    flushSync();

    options()[0]?.click();
    flushSync();

    expect(
      answers(),
      'and the refused take does not re-arm the row it was answered with',
    ).toHaveLength(1);
    expect(
      document.querySelector('.dock .answering'),
      'the stand-down is still standing while the answer is out',
    ).not.toBeNull();
  });

  /** Press the dock's mic and let the take it starts settle. */
  async function micPress(): Promise<void> {
    const button = document.querySelector('.dock .micb');
    if (!(button instanceof HTMLElement)) throw new Error('the dock drew no mic');
    button.click();
    for (let turn = 0; turn < 4; turn += 1) await Promise.resolve();
    flushSync();
  }

  /** Swap the dock drawn for another, which is one kind after another. */
  function again(held: unknown, dictation: boolean): void {
    if (app !== null) void unmount(app);
    app = null;
    document.body.innerHTML = '';
    open({ record: record({ pending_asks: [held] }), dictation });
  }

  /**
   * The mic, which is the spec's own AUQ case: this surface's door to a take,
   * and the words it produces land in the row it was pressed on.
   *
   * Both kinds with a words row, because the gate and the landing are each a
   * line of the same markup: a pin driving only a question says nothing about
   * the permission that draws the same mic.
   */
  it('draws the mic only where this install can dictate', () => {
    for (const held of [questionAsk(), permissionAsk()]) {
      again(held, false);
      expect(
        document.querySelector('.dock .micb'),
        'a control this install cannot honour is worse than none',
      ).toBeNull();

      again(held, true);
      expect(document.querySelector('.dock .micb'), 'and the door where it can').not.toBeNull();
    }
  });

  it('draws no mic on a held post, whose text is approved rather than composed', () => {
    open({ record: record({ pending_asks: [slackDraftAsk()] }), dictation: true });

    expect(
      document.querySelector('.dock .micb'),
      'the words are approved, not composed, and the box is where they are edited',
    ).toBeNull();
  });

  it('starts a take from the mic and ends it there', async () => {
    vi.useFakeTimers();
    try {
      const harness = open({ record: record({ pending_asks: [questionAsk()] }), dictation: true });

      await micPress();
      expect(
        harness.sent.map((entry) => entry.command),
        'the press opens the microphone',
      ).toMatchObject([
        { dictate_stream: { key: { org: 'Busytools', project: 'forge', label: 'lead' } } },
      ]);

      await micPress();
      // The release drains the pipeline before its stop goes out.
      settle();
      expect(harness.sent.at(-1)?.command, 'and the second press submits it').toMatchObject({
        dictate_stop: { key: { org: 'Busytools', project: 'forge', label: 'lead' }, submit: true },
      });
    } finally {
      vi.useRealTimers();
    }
  });

  it("lands a take's words in the words row, whichever kind holds the slot", async () => {
    for (const held of [questionAsk(), permissionAsk()]) {
      const harness = open({ record: record({ pending_asks: [held] }), dictation: true });

      await micPress();
      await micPress();

      // The server says the take is running, then that it landed: the box has to
      // have watched it for the landing to be its own.
      harness.page.record = record({
        pending_asks: [held],
        composer: { take: take(), notice: null, machine: null },
      });
      flushSync();
      harness.page.record = record({
        pending_asks: [held],
        composer: {
          take: null,
          notice: { kind: 'landed', text: 'the dictated answer', truncated: false },
          machine: null,
        },
      });
      flushSync();

      const field = document.querySelector('.dock textarea.notes');
      expect(
        field instanceof HTMLTextAreaElement ? field.value : null,
        'the words land in the row the mic started them from, not in a box this slot is not drawing',
      ).toBe('the dictated answer');

      if (app !== null) void unmount(app);
      app = null;
      document.body.innerHTML = '';
    }
  });

  /**
   * **The dock's words are marked taken, so a re-hand appends them once**
   * (#1360). The dock path was the one placement that never marked the
   * memory: a whole-record read re-handing the same notice appended the
   * words the dock already held, and submitting sent them doubled.
   */
  it('marks the words the dock took, so a carried re-hand appends nothing', async () => {
    const held = questionAsk();
    const harness = open({ record: record({ pending_asks: [held] }), dictation: true });
    await micPress();
    await micPress();
    harness.page.record = record({
      pending_asks: [held],
      composer: { take: take(), notice: null, machine: null },
    });
    flushSync();
    harness.page.record = record({
      pending_asks: [held],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'the dictated answer', truncated: false },
        machine: null,
      },
    });
    flushSync();
    const field = document.querySelector('.dock textarea.notes');
    if (!(field instanceof HTMLTextAreaElement)) throw new Error('the dock drew no notes');
    expect(field.value, 'the live landing reached the dock').toBe('the dictated answer');

    // The seat re-hands the same words as carried ones on a whole-record read.
    harness.page.record = record({
      pending_asks: [held],
      composer: {
        take: null,
        notice: {
          kind: 'landed',
          text: 'the dictated answer',
          truncated: false,
          carried: true,
        },
        machine: null,
      },
    });
    flushSync();
    expect(field.value, 'and the re-hand appends nothing: the dock already took these words').toBe(
      'the dictated answer',
    );

    if (app !== null) void unmount(app);
    app = null;
    document.body.innerHTML = '';
  });

  it('refuses a mic press on a seat whose take belongs to another', async () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }), dictation: true });
    await micPress();

    // The page moves to another seat, whose own prompt draws the same mic.
    harness.page.slot = ELSEWHERE;
    harness.page.record = record({ slot: ELSEWHERE, pending_asks: [questionAsk()] });
    flushSync();

    const sent = harness.sent.length;
    await micPress();

    expect(harness.sent.length, 'the press stops nothing it cannot see').toBe(sent);
    expect(drawn(), 'and it is refused by name, as the key path refuses it').toContain(
      'the microphone is in use',
    );
  });

  it('answers the row a number names, which is the number the row draws', () => {
    const harness = open({
      record: record({ pending_asks: [questionAsk('tu-q', { multi_select: false })] }),
    });

    expect(
      [...document.querySelectorAll('.dock .opt .n')].map((held) => held.textContent),
      'the rows carry their own numbers, the first row one, the words row last',
    ).toEqual(['1', '2', '3']);

    press('2');

    expect(commands(harness), 'and the key answers the row it names').toEqual([
      {
        respond_question: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          tool_id: 'tu-q',
          outcome: { outcome: 'answered', selected_option_ids: ['q-prod'], annotation: null },
        },
      },
    ]);
  });

  it('draws the wait on the row that was answered, not under the list', () => {
    const harness = open({
      record: record({ pending_asks: [questionAsk('tu-q', { multi_select: false })] }),
    });

    options()[1]?.click();
    flushSync();
    expect(commands(harness), 'the answer went').toHaveLength(1);

    expect(
      document.querySelector('.dock .opt .answering'),
      'the wait rides the row it came from',
    ).not.toBeNull();
    expect(
      document.querySelector('.dock .keys .answering'),
      'and the line under the list stays for the answers with no row',
    ).toBeNull();
  });

  /**
   * A rejection's wait belongs to the rejection.
   *
   * The row a REFUSED answer named is not the row the next answer came from, so
   * the row the dock holds has to be cleared when the reader rejects instead -
   * otherwise the wait for the rejection draws on a row nobody answered with.
   */
  it("draws a rejected answer's wait under the list, not on the refused answer's row", () => {
    const harness = open({
      record: record({ pending_asks: [questionAsk('tu-q', { multi_select: false })] }),
    });

    options()[1]?.click();
    flushSync();

    // The core refuses it, so the dock comes back live with the reason.
    harness.say({ kind: 'error', what: 'dispatch', why: 'the prompt was already answered' });
    flushSync();

    press('Escape');
    flushSync();

    expect(commands(harness).at(-1), 'the rejection went').toMatchObject({
      respond_question: { outcome: { outcome: 'cancelled' } },
    });
    expect(
      document.querySelector('.dock .keys .answering'),
      'the wait draws under the list, where an answer with no row rides',
    ).not.toBeNull();
    expect(
      document.querySelector('.dock .opt .answering'),
      'and not on the row the refused answer named',
    ).toBeNull();
  });

  it('draws the wait under the list for an answer that came from the words', () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    const field = document.querySelector('.dock textarea.notes');
    if (!(field instanceof HTMLTextAreaElement)) throw new Error('the dock drew no words row');
    field.focus();
    field.value = 'neither, wait for the release';
    field.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    press('Enter');

    expect(commands(harness), 'the words went').toHaveLength(1);
    expect(
      document.querySelector('.dock .keys .answering'),
      'an answer with no row has no row to ride',
    ).not.toBeNull();
    expect(document.querySelector('.dock .opt .answering')).toBeNull();
  });

  it('holds the mark and the toggles while an answer is on its way', () => {
    open({ record: record({ pending_asks: [questionAsk()] }) });

    const list = document.querySelector('.dock [role="listbox"]');
    if (!(list instanceof HTMLElement)) throw new Error('the dock drew no listbox');

    press(' ');
    flushSync();
    const boxes = () =>
      [...document.querySelectorAll('.dock .box2')].map((box) => box.classList.contains('on'));
    expect(boxes(), 'the first row is on').toEqual([true, false]);

    // The answer goes, and the dock stands down: the rows are a record of it
    // now, not a list to keep working.
    press('Enter');
    flushSync();

    list.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true }));
    list.dispatchEvent(new KeyboardEvent('keydown', { key: ' ', bubbles: true }));
    flushSync();

    expect(
      document.querySelector('.dock .opt.sel')?.textContent,
      'the mark does not move on an answered prompt',
    ).toContain('Staging');
    expect(boxes(), 'and space flips nothing behind it').toEqual([true, false]);
  });

  it('draws one primary action, the trusting last one', () => {
    // The wire's common prompt carries two allows: marking both is no
    // hierarchy, so the last one - the most trusting - is the primary.
    open({
      record: record({
        pending_asks: [
          {
            kind: 'permission',
            request: {
              tool_call: {
                tool_call_id: 'tu-2',
                title: 'Bash',
                kind: 'execute',
                status: 'pending',
                content: [],
                locations: [],
                raw_input: { command: 'git push origin main' },
              },
              display: {
                title: 'Bash',
                display_name: null,
                description: null,
                decision_reason: null,
              },
              options: [
                {
                  option_id: 'opt-once',
                  name: 'Allow once',
                  kind: 'allow',
                  action: { kind: 'allow' },
                },
                {
                  option_id: 'opt-always',
                  name: 'Allow always',
                  kind: 'allow',
                  action: { kind: 'allow' },
                },
                { option_id: 'opt-deny', name: 'Deny', kind: 'deny', action: { kind: 'deny' } },
              ],
            },
          },
        ],
      }),
    });

    const primaries = [...document.querySelectorAll('.dock .acts .btn.p')];
    expect(primaries, 'exactly one action is primary').toHaveLength(1);
    expect(primaries[0]?.textContent, 'and it is the trusting one').toContain('Allow always');
  });

  it('prefers the description over the command when the wire carries both', () => {
    open({
      record: record({
        pending_asks: [
          {
            kind: 'permission',
            request: {
              tool_call: {
                tool_call_id: 'tu-3',
                title: 'Bash',
                kind: 'execute',
                status: 'pending',
                content: [],
                locations: [],
                raw_input: {
                  description: 'Push the release tag once the wave lands',
                  command: 'git push origin v1.0.115',
                },
              },
              display: {
                title: 'Bash',
                display_name: null,
                description: null,
                decision_reason: null,
              },
              options: [
                {
                  option_id: 'opt-once',
                  name: 'Allow once',
                  kind: 'allow',
                  action: { kind: 'allow' },
                },
              ],
            },
          },
        ],
      }),
    });

    expect(
      document.querySelector('.dock .d-q')?.textContent,
      "the description is the server's own pick, and the command is its fallback",
    ).toBe('Push the release tag once the wave lands');
  });

  it('keeps a take visible behind a held post, which draws no words row', () => {
    open({
      record: record({
        pending_asks: [slackDraftAsk()],
        composer: { take: take(), notice: null, machine: null },
      }),
    });

    expect(
      document.querySelector('.dock .blip'),
      'a post cannot swallow a recording that is still going',
    ).not.toBeNull();
    expect(drawn()).toContain('dictating');
  });

  it('says on the submit key how many rows will go', () => {
    open({ record: record({ pending_asks: [questionAsk()] }) });

    options()[0]?.click();
    options()[1]?.click();
    flushSync();

    expect(drawn(), 'a set is never submitted unseen').toContain('submit 2');
  });

  it("draws a permission's subject as it arrives, prose or command", () => {
    const harness = open({ record: record({ pending_asks: [permissionAsk()] }) });
    expect(
      document.querySelector('.dock .d-q')?.textContent,
      'the command the fixture carries',
    ).toBe('git push origin main');

    // The same wire with Claude's own description instead of a command: the
    // server prefers the description, and this side reads the subject its way.
    harness.page.record = record({
      pending_asks: [
        {
          kind: 'permission',
          request: {
            tool_call: {
              tool_call_id: 'tu-2',
              title: 'Bash',
              kind: 'execute',
              status: 'pending',
              content: [],
              locations: [],
              raw_input: { description: 'Push the release tag once the wave lands' },
            },
            display: {
              title: 'Bash',
              display_name: null,
              description: null,
              decision_reason: null,
            },
            options: [
              {
                option_id: 'opt-once',
                name: 'Allow once',
                kind: 'allow',
                action: { kind: 'allow' },
              },
            ],
          },
        },
      ],
    });
    flushSync();

    expect(
      document.querySelector('.dock .d-q')?.textContent,
      'and prose is drawn as prose rather than dressed as a command',
    ).toBe('Push the release tag once the wave lands');
  });
});

/**
 * The takeover bar's feedback line: the one words row the bar owns, and the
 * only place a take pressed there can say what it is doing - the composer's
 * band and notice rows are behind the picture while the takeover is up.
 */
describe("the takeover bar's feedback line", () => {
  const commands = (harness: { sent: { command: Record<string, unknown> }[] }) =>
    harness.sent.map((entry) => entry.command);

  /** **The bar's mic is the composer's take, not a second one.** The feedback
   *  line draws the dock's own words row, take card and mic, so the press there
   *  has to open the microphone exactly as the box's mic does. The case passes
   *  on the tree before this cut too: what it pins is the contract the fix
   *  keeps, not the defect. */
  it('starts a take from the bar feedback line, and ends it there', async () => {
    vi.useFakeTimers();
    try {
      const harness = open({
        record: record({ pending_asks: [browserHandOffAsk()] }),
        dictation: true,
      });

      action('Open browser').click();
      await opened();
      barAction('something').click();
      flushSync();
      const micButton = document.querySelector('.takeover-bar .micb');
      if (!(micButton instanceof HTMLElement)) throw new Error('the bar drew no mic');

      micButton.click();
      await opened();
      expect(commands(harness), 'the press opens the microphone').toMatchObject([
        { dictate_stream: { key: SLOT } },
      ]);

      micButton.click();
      settle();
      expect(commands(harness).at(-1), 'and the second press submits it').toMatchObject({
        dictate_stop: { key: SLOT, submit: true },
      });
    } finally {
      vi.useRealTimers();
    }
  });

  /** **The key is the bar's second door.** The takeover owns the keyboard for
   *  the page under it, but the feedback line is this client's own box: with
   *  the caret in it a press of the push-to-talk key begins the take there
   *  exactly as the mic does - and its words land on that line, because the
   *  bar is the box the take began in. With the caret anywhere else the page
   *  keeps the keyboard (`push-to-talk.test.ts`). */
  it('starts a take from the bar line on the push-to-talk key, and its words land there', async () => {
    const harness = open({
      record: record({
        pending_asks: [browserHandOffAsk()],
        composer: { take: null, notice: null, machine: null, bind: 'right_cmd', mode: 'auto' },
      }),
      dictation: true,
    });

    action('Open browser').click();
    await opened();
    barAction('something').click();
    flushSync();
    const line = document.querySelector<HTMLTextAreaElement>('.takeover-bar textarea');
    if (!(line instanceof HTMLTextAreaElement)) throw new Error('the bar drew no feedback line');

    line.focus();
    key('ControlRight', 'keydown');
    await opened();
    expect(commands(harness), 'the key opens the microphone at the bar').toMatchObject([
      { dictate_stream: { key: SLOT } },
    ]);

    landBarTake(harness);
    expect(
      document.querySelector<HTMLTextAreaElement>('.takeover-bar textarea')?.value,
      'the words land where the key was pressed',
    ).toBe('the words');
  });

  /** **The words left, so the chat gets nothing back.** A dictated line is a
   *  landed take, and the ask resolving runs the carry - so the words the
   *  agent just read were written into the composer's draft after the send.
   *  A send spends the line: it clears, and the resolution finds nothing to
   *  hand back. */
  it('sends a dictated line and leaves none of it in the chat', async () => {
    vi.mocked(showBrowser).mockResolvedValueOnce('the browser host exited');
    const harness = open({
      dictation: true,
      record: record({ pending_asks: [browserHandOffAsk()] }),
    });
    action('Open browser').click();
    await vi.waitFor(() => expect(surface.phase, 'the page failed').toBe('failed'));
    flushSync();
    barAction('something').click();
    flushSync();
    await startBarTake();
    landBarTake(harness);

    const send = document.querySelector('.takeover-bar .sendb');
    if (!(send instanceof HTMLElement)) throw new Error('the bar drew no send');
    const before = commands(harness).length;
    send.click();
    // The ending answers only once the connection says the prompt crossed.
    await vi.waitFor(() =>
      expect(
        commands(harness)
          .slice(before)
          .map((command) => Object.keys(command)[0]),
        'the words go to the session, and the hand-off ends as feedback',
      ).toEqual(['prompt_under', 'respond_browser_hand_off']),
    );
    flushSync();
    const [prompt] = commands(harness).slice(before);
    expect(
      (prompt?.['prompt_under'] as Record<string, unknown> | undefined)?.['text'],
      'the dictated words are the words the agent reads',
    ).toBe('the words');
    await vi.waitFor(() => expect(surface.phase, 'the chat is back').toBe('closed'));
    flushSync();

    harness.page.record = record({ pending_asks: [] });
    flushSync();
    expect(field().value, 'the resolved hand-off hands nothing back').toBe('');
  });

  /** **Words that never left stay where they were.** A send the socket
   *  refused is a press with nothing behind it: the line keeps what was
   *  dictated so the reader can send it again, and no clear may run ahead of
   *  the dispatch's own refusal. */
  it('keeps a dictated line on the bar when the send never left', async () => {
    const shared = wire();
    vi.mocked(showBrowser).mockResolvedValueOnce('the browser host exited');
    const harness = open(
      { dictation: true, record: record({ pending_asks: [browserHandOffAsk()] }) },
      shared,
    );
    action('Open browser').click();
    await vi.waitFor(() => expect(surface.phase, 'the page failed').toBe('failed'));
    flushSync();
    barAction('something').click();
    flushSync();
    await startBarTake();
    landBarTake(harness);

    const send = document.querySelector('.takeover-bar .sendb');
    if (!(send instanceof HTMLElement)) throw new Error('the bar drew no send');
    const before = commands(harness).length;
    shared.down = true;
    try {
      send.click();
      flushSync();
    } finally {
      shared.down = false;
    }

    expect(commands(harness).slice(before), 'a send the socket refused answers nothing').toEqual(
      [],
    );
    expect(
      document.querySelector<HTMLTextAreaElement>('.takeover-bar textarea')?.value,
      'and the words stay on the line, to be sent again',
    ).toBe('the words');

    // The refused press spent nothing: lowering the bar still hands the words
    // to the chat, exactly as it does when no send was attempted.
    barAction('back to chat').click();
    await vi.waitFor(() => expect(surface.phase, 'the chat is back').toBe('closed'));
    flushSync();
    expect(drawn(), 'and lowering the bar hands them to the chat').toContain(
      "What you said at the bar is in the chat's draft: the words",
    );
  });

  /** **The card is this page's take, not the record's echo.** A take whose
   *  stream has not left yet - a socket still opening under the press - has no
   *  record to draw from, and the composer's own row draws it from `takePhase`
   *  anyway; a bar reading only the record drew the empty field for as long as
   *  the socket stayed down, which is the dead-looking press again. */
  it("draws the card from this page's own take before the record has it", async () => {
    const shared = wire();
    open({ record: record({ pending_asks: [browserHandOffAsk()] }), dictation: true }, shared);

    action('Open browser').click();
    await opened();
    barAction('something').click();
    flushSync();
    const button = document.querySelector('.takeover-bar .micb');
    if (!(button instanceof HTMLElement)) throw new Error('the bar drew no mic');

    shared.down = true;
    try {
      button.click();
      await opened();
    } finally {
      shared.down = false;
    }

    expect(
      document.querySelector('.tb-feedback .tc'),
      'the page holds a take, so the bar draws its row',
    ).not.toBeNull();
    expect(document.querySelector('.tb-field'), 'and the field it replaces is gone').toBeNull();
  });

  /** **A take that will not open says so where it was pressed.** The bar's
   *  feedback row is the words row a take here belongs to, and the composer's
   *  own notice row is behind the picture while the takeover is up - so a
   *  refusal routed there was a press that looked dead, while the same press
   *  in the box drew its line. */
  it('says in the bar why a take pressed there did not open', async () => {
    open({ record: record({ pending_asks: [browserHandOffAsk()] }), dictation: true });

    action('Open browser').click();
    await opened();
    barAction('something').click();
    flushSync();
    const button = document.querySelector('.takeover-bar .micb');
    if (!(button instanceof HTMLElement)) throw new Error('the bar drew no mic');

    mic.held.refuse = true;
    try {
      button.click();
      await opened();
    } finally {
      mic.held.refuse = false;
    }

    expect(
      document.querySelector('.takeover-bar')?.textContent,
      'the bar names the refusal where the press was made',
    ).toContain('the microphone was refused');
    // **The reader's own typing dismisses the line**, as it does the
    // composer's notice: the bar's field is where that typing lands.
    const field = document.querySelector('.takeover-bar textarea');
    if (!(field instanceof HTMLTextAreaElement)) throw new Error('the bar drew no field');
    field.value = 'the upload form rejected the file';
    field.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    expect(
      document.querySelector('.takeover-bar')?.textContent,
      'and a line typed away does not keep drawing',
    ).not.toContain('the microphone was refused');
  });

  /** **The open itself is the bar's to say.** The card comes from this page's
   *  own take, which exists only once the capture has opened, and the
   *  composer's own band is behind the picture while the takeover is up - so
   *  an open that takes seconds drew nothing at all, and the press read as
   *  dead (measured live: a 4s open showed no card, no line and no status on
   *  this surface, while the composer's band said it within 200ms).
   *  **The wait is pinned at the 180ms `OPEN_DRAWN_MS` sets, not at whatever
   *  the constant holds today**: the band's delay is what stops a fast press
   *  flashing the state, and a "make it snappier" edit is the change it exists
   *  to refuse. */
  it('says in the bar that a take pressed there is still opening', async () => {
    vi.useFakeTimers();
    mic.held.gated = true;
    try {
      const harness = open({
        record: record({ pending_asks: [browserHandOffAsk()] }),
        dictation: true,
      });

      action('Open browser').click();
      await opened();
      barAction('something').click();
      flushSync();
      const button = document.querySelector('.takeover-bar .micb');
      if (!(button instanceof HTMLElement)) throw new Error('the bar drew no mic');

      button.click();
      flushSync();
      vi.advanceTimersByTime(179);
      flushSync();
      expect(
        document.querySelector('.takeover-bar')?.textContent,
        'an open inside the 180ms wait is the press itself, not a state to flash',
      ).not.toContain('opening the microphone');

      vi.advanceTimersByTime(1);
      flushSync();
      expect(
        document.querySelector('.takeover-bar')?.textContent,
        'the band the composer draws is behind the picture, so this row says it',
      ).toContain('opening the microphone');
      expect(
        document.querySelector('.tb-feedback .tc'),
        'and no card claims a take that does not exist yet',
      ).toBeNull();

      mic.held.release?.();
      await opened();
      expect(commands(harness), 'the open lands, and the take goes out').toMatchObject([
        { dictate_stream: { key: SLOT } },
      ]);
      expect(
        document.querySelector('.takeover-bar')?.textContent,
        'and the line goes with it',
      ).not.toContain('opening the microphone');
      expect(
        document.querySelector('.tb-feedback .tc'),
        'the card replaces the line rather than leaving the row empty',
      ).not.toBeNull();
    } finally {
      mic.held.gated = false;
      mic.held.release = null;
      vi.useRealTimers();
    }
  });

  /** **The bar's line is dictatable without a click.** A reader driving the
   *  page under the takeover leaves the caret on the page - the body, not one
   *  of this client's boxes - and the key presses there were dropped unless
   *  the reader had first clicked into the feedback field. The press has to
   *  begin the bar's take instead: its line opens if it was shut, and its
   *  field takes the keyboard so the words have a home and the release is the
   *  natural one. */
  it('begins the bar take from the key with the caret on the page, and takes the keyboard', async () => {
    mic.held.gated = true;
    try {
      const harness = open({
        record: record({ pending_asks: [browserHandOffAsk()] }),
        dictation: true,
      });
      action('Open browser').click();
      await opened();
      expect(
        document.querySelector('.takeover-bar textarea'),
        'precondition: the feedback line is shut',
      ).toBeNull();
      (document.activeElement as HTMLElement | null)?.blur();
      expect(document.activeElement, 'precondition: the caret sits on the page').toBe(
        document.body,
      );

      key('ControlRight', 'keydown');
      await opened();

      const line = document.querySelector<HTMLTextAreaElement>('.takeover-bar textarea');
      expect(
        line,
        'the press opens the bar line, rather than landing where nothing types',
      ).not.toBeNull();
      expect(document.activeElement, 'and its field holds the keyboard').toBe(line);

      mic.held.release?.();
      await opened();
      expect(commands(harness), 'the take opens at the bar').toMatchObject([
        { dictate_stream: { key: SLOT } },
      ]);
    } finally {
      mic.held.gated = false;
      mic.held.release = null;
    }
  });

  /** **A take in flight ends on the key wherever the caret is.** Beginning the
   *  take swaps the line's field for its card, and the swap is what swallowed
   *  the stop: the caret fell to the body, which is not a dictation box, so
   *  the press was dropped and the reader had to reach for the mic.
   *
   *  **What this is evidence for is the finish decision**: the bar's own take,
   *  before the record has echoed it, still counts as in flight. The takeover
   *  guard's live-take clause is `push-to-talk.test.ts`'s - here the bar line
   *  would let the press through on its own. */
  it('ends the bar take from the key with the caret on the page, while the card draws', async () => {
    vi.useFakeTimers();
    try {
      const harness = open({
        record: record({ pending_asks: [browserHandOffAsk()] }),
        dictation: true,
      });
      action('Open browser').click();
      await opened();
      barAction('something').click();
      flushSync();
      await startBarTake();
      expect(
        document.querySelector('.tb-feedback .tc'),
        'precondition: the card replaces the line',
      ).not.toBeNull();
      expect(
        document.querySelector('.takeover-bar textarea'),
        'precondition: no field holds the caret',
      ).toBeNull();
      (document.activeElement as HTMLElement | null)?.blur();
      expect(document.activeElement, 'precondition: the caret sits on the page').toBe(
        document.body,
      );

      key('ControlRight', 'keydown');
      key('ControlRight', 'keyup');
      settle();

      expect(
        commands(harness).at(-1),
        'the press ends the take rather than beginning one behind the page',
      ).toMatchObject({ dictate_stop: { key: SLOT, submit: true } });
      expect(
        commands(harness).filter((command) => 'dictate_stream' in command),
        'and no second take opens',
      ).toHaveLength(1);
    } finally {
      vi.useRealTimers();
    }
  });

  /** **One press is one act.** The bar's line is a dictation box on the same
   *  page as the composer's, so a second registration hearing the same press
   *  would stop the take twice.
   *
   *  **The take here is the record's, which this page did not start**: each
   *  handler's finish goes to the socket rather than collapsing on one local
   *  capture's drain, so the stop count is the honest one a duplicate
   *  registration would move. */
  it('stops once from one press under the takeover', async () => {
    const harness = open({
      record: record({
        pending_asks: [browserHandOffAsk()],
        composer: { take: take(), notice: null, machine: null, bind: 'right_cmd', mode: 'toggle' },
      }),
      dictation: true,
    });
    action('Open browser').click();
    await opened();
    barAction('something').click();
    flushSync();

    key('ControlRight', 'keydown');
    key('ControlRight', 'keyup');

    expect(
      commands(harness).filter((command) => 'dictate_stop' in command),
      'the press stops the seat take exactly once',
    ).toHaveLength(1);
  });

  /** **A line the reader cannot close is not a door for the key.** While the
   *  page is still opening the bar draws no toggle, so a press that opened the
   *  feedback line there would leave words in a row with no way to shut it. */
  it('begins nothing while the page has no line to close', async () => {
    let land: (answer: { profile: string; tab: number }) => void = () => {};
    vi.mocked(showBrowser).mockReturnValueOnce(
      new Promise((resolve) => {
        land = resolve;
      }),
    );
    const harness = open({
      record: record({ pending_asks: [browserHandOffAsk()] }),
      dictation: true,
    });
    action('Open browser').click();
    flushSync();
    expect(surface.phase, 'precondition: the page is still opening').toBe('opening');
    (document.activeElement as HTMLElement | null)?.blur();

    key('ControlRight', 'keydown');
    await opened();

    expect(commands(harness), 'the press opens no take under a page with no controls').toEqual([]);
    expect(
      document.querySelector('.takeover-bar textarea'),
      'and no line is drawn for it',
    ).toBeNull();

    land({ profile: 'job-hunt', tab: 2 });
    await raised();
  });

  /** **The overlay's bare modifier is the key too.** While the overlay holds
   *  first responder the webview gets no keydown, so a person pressing the
   *  binding on the page reached nothing at all. The shell forwards the raw
   *  side and the page decides which one is bound. */
  it("begins the bar take from the overlay's modifier with the caret on the page", async () => {
    const harness = open({
      record: record({ pending_asks: [browserHandOffAsk()] }),
      dictation: true,
    });
    action('Open browser').click();
    await opened();
    const pressed = await modifierRoute();
    (document.activeElement as HTMLElement | null)?.blur();
    expect(document.activeElement, 'precondition: the caret sits on the page').toBe(document.body);

    pressed({ code: 62, down: true });
    flushSync();
    pressed({ code: 62, down: false });
    await opened();

    expect(commands(harness), 'the overlay press begins the bar take').toMatchObject([
      { dictate_stream: { key: SLOT } },
    ]);
    expect(
      document.querySelector('.tb-feedback .tc'),
      'and the bar draws the take it opened for',
    ).not.toBeNull();
  });

  /** **The overlay's release ends it too.** A take in flight is the stop
   *  wherever the caret is; the shell's route has to land on the same finish
   *  the window key does, or a person driving the page can start a take they
   *  cannot end. */
  it("ends a live bar take from the overlay's modifier", async () => {
    vi.useFakeTimers();
    try {
      const harness = open({
        record: record({ pending_asks: [browserHandOffAsk()] }),
        dictation: true,
      });
      action('Open browser').click();
      await opened();
      barAction('something').click();
      flushSync();
      await startBarTake();
      const pressed = await modifierRoute();
      (document.activeElement as HTMLElement | null)?.blur();

      pressed({ code: 62, down: true });
      flushSync();
      pressed({ code: 62, down: false });
      settle();

      expect(commands(harness).at(-1), 'the overlay press ends the take').toMatchObject({
        dictate_stop: { key: SLOT, submit: true },
      });
    } finally {
      vi.useRealTimers();
    }
  });

  /** **A chord heard on the overlay's route discards the take it began.** The
   *  page never sees Cmd+C while the overlay holds the keyboard, so the shell
   *  reports the other key; without it the release would submit a take the
   *  person meant as a shortcut. */
  it("discards the take when a chord arrives on the overlay's route", async () => {
    vi.useFakeTimers();
    try {
      const harness = open({
        record: record({ pending_asks: [browserHandOffAsk()] }),
        dictation: true,
      });
      action('Open browser').click();
      await opened();
      const pressed = await modifierRoute();
      const chord = await modifierChordRoute();

      pressed({ code: 62, down: true });
      await opened();
      chord();
      pressed({ code: 62, down: false });
      settle();

      expect(
        commands(harness).at(-1),
        'the release discards the take rather than submitting it',
      ).toMatchObject({ dictate_stop: { key: SLOT, submit: false } });
    } finally {
      vi.useRealTimers();
    }
  });

  /** **One press is one act, whichever route delivered it.** The webview gets
   *  no keydown while the overlay is first responder, but if a shell ever
   *  forwarded both, they land in ONE handler pair - and a second press while
   *  one is already down carries no instruction.
   *
   *  **The take is the record's and the mode auto**, which is what makes the
   *  count discriminating: a separate pair would track its own press and send
   *  its own finish, where the shared pair's second release has nothing left
   *  to end. */
  it('acts once when the overlay and the window both deliver the press', async () => {
    const harness = open({
      record: record({
        pending_asks: [browserHandOffAsk()],
        composer: { take: take(), notice: null, machine: null, bind: 'right_cmd', mode: 'auto' },
      }),
      dictation: true,
    });
    action('Open browser').click();
    await opened();
    const pressed = await modifierRoute();

    pressed({ code: 62, down: true });
    key('ControlRight', 'keydown');
    key('ControlRight', 'keyup');
    pressed({ code: 62, down: false });
    await opened();

    expect(
      commands(harness).filter((command) => 'dictate_stop' in command),
      'both routes end the seat take exactly once',
    ).toHaveLength(1);
    expect(
      commands(harness).filter((command) => 'dictate_stream' in command),
      'and nothing begins a take behind the page',
    ).toHaveLength(0);
  });

  /** **The field comes back with the words and the keyboard with it.** A take
   *  lands its words in the line and the line's field returns; a reader who
   *  just dictated them is about to press Enter or type, and a keystroke on
   *  the body goes nowhere.
   *
   *  **The row is the record's phase, not this page's take.** A release leaves
   *  the record transcribing for a drain after the local capture has gone, so
   *  the field only returns when the record clears; the sequence below is the
   *  production one, a flush apart, because a focus keyed on the local take
   *  fired while the card was still drawn and never ran again. */
  it("gives the keyboard back to the bar's field when the take row clears", async () => {
    const harness = open({
      record: record({ pending_asks: [browserHandOffAsk()] }),
      dictation: true,
    });
    action('Open browser').click();
    await opened();
    barAction('something').click();
    flushSync();
    await startBarTake();

    // The release: the record shows the take transcribing while the local
    // capture is still ending.
    harness.page.record = record({
      pending_asks: [browserHandOffAsk()],
      composer: { take: take({ phase: 'transcribing' }), notice: null, machine: null },
    });
    flushSync();
    endTake(harness);
    flushSync();
    // **The record transcribes for a beat after the local end**, as a server
    // update does; the focus this page's own take queued has to have died
    // with the field by then, or the later clear finds nothing to drive it.
    await opened();
    expect(
      document.querySelector('.takeover-bar textarea'),
      'the card still holds the line while the record transcribes',
    ).toBeNull();
    expect(
      document.activeElement,
      'and the keyboard is in the bar the takeover owns, not the field the card replaced',
    ).toBe(document.querySelector('.takeover-bar'));

    // The record clears a flush later: the field returns with the words.
    harness.page.record = record({
      pending_asks: [browserHandOffAsk()],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'the words', truncated: false },
        machine: null,
      },
    });
    flushSync();

    const line = document.querySelector<HTMLTextAreaElement>('.takeover-bar textarea');
    expect(line, "the line's field returns with the take's words").not.toBeNull();
    expect(line?.value, 'and it holds what was dictated').toBe('the words');
    await vi.waitFor(() =>
      expect(document.activeElement, 'the keyboard is in the field the words landed in').toBe(line),
    );
  });

  /** **An install that cannot dictate opens no overlay key routes.** The
   *  shell's doors sit behind the same gate the window listener does. */
  it('opens no overlay key route on an install that cannot dictate', async () => {
    open({ record: record({ pending_asks: [browserHandOffAsk()] }), dictation: false });
    action('Open browser').click();
    await opened();
    await opened();

    expect(vi.mocked(onModifier), 'no modifier door is opened').not.toHaveBeenCalled();
    expect(vi.mocked(onModifierChord), 'and no chord door either').not.toHaveBeenCalled();
  });

  /** **A refused event door is said, and the next run opens it again.** A
   *  route that dies in silence is a key that stops working with no line
   *  anywhere. */
  it('says when the overlay key door could not open, and opens it on the next run', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    try {
      vi.mocked(onModifier).mockReturnValueOnce(
        Promise.reject(new Error('the event plugin was not ready')),
      );
      open({ record: record({ pending_asks: [browserHandOffAsk()] }), dictation: true });
      action('Open browser').click();
      await opened();

      await vi.waitFor(() =>
        expect(
          warn.mock.calls.some((call) =>
            String(call[0]).includes('overlay key route did not open'),
          ),
          'the refusal is said rather than swallowed',
        ).toBe(true),
      );
      expect(
        vi.mocked(onModifier).mock.calls.length,
        'the next run of the door opens it again',
      ).toBeGreaterThan(1);
    } finally {
      warn.mockRestore();
    }
  });

  /** **A press whose release landed while the window was away is forgotten.**
   *  Without the reset the press stays on record, the next key is taken for a
   *  repeat, and the key looks dead. */
  it('begins again after a press whose release was lost', async () => {
    vi.useFakeTimers();
    try {
      const harness = open({
        record: record({
          pending_asks: [browserHandOffAsk()],
          composer: { take: null, notice: null, machine: null, bind: 'right_cmd', mode: 'toggle' },
        }),
        dictation: true,
      });
      action('Open browser').click();
      await opened();
      barAction('something').click();
      flushSync();

      key('ControlRight', 'keydown');
      await opened();
      expect(
        commands(harness).filter((command) => 'dictate_stream' in command),
        'the press began a take',
      ).toHaveLength(1);

      window.dispatchEvent(new Event('blur'));
      key('ControlRight', 'keydown');
      settle();

      expect(
        commands(harness).filter((command) => 'dictate_stop' in command),
        'the next press stops the take rather than being taken for a repeat',
      ).toHaveLength(1);
    } finally {
      vi.useRealTimers();
    }
  });
});

/**
 * The handler the shell's modifier door carried, once the takeover is up and
 * the composer has opened it.
 *
 * **jsdom has no responder chain**, so nothing here proves AppKit delivers
 * `flagsChanged:` - the shell's own test pins the payload it emits. What the
 * cases below pin is the client's half: the door is opened only while the
 * takeover is up, and what arrives through it goes into the same handler pair
 * the window key does.
 */
async function modifierRoute(): Promise<(press: { code: number; down: boolean }) => void> {
  await vi.waitFor(() =>
    expect(
      vi.mocked(onModifier),
      'the overlay key door is opened while the takeover is up',
    ).toHaveBeenCalled(),
  );
  const call = vi.mocked(onModifier).mock.calls.at(-1);
  if (call === undefined) throw new Error('the overlay key door was never opened');
  return call[0];
}

/** The handler the shell's chord door carried, once the takeover is up. */
async function modifierChordRoute(): Promise<() => void> {
  await vi.waitFor(() =>
    expect(
      vi.mocked(onModifierChord),
      'the overlay chord door is opened while the takeover is up',
    ).toHaveBeenCalled(),
  );
  const call = vi.mocked(onModifierChord).mock.calls.at(-1);
  if (call === undefined) throw new Error('the overlay chord door was never opened');
  return call[0];
}

/**
 * The list that opens over the box: five triggers, one popover shape.
 */
describe('the autocomplete', () => {
  it('opens over the box on the trigger the draft ends in, with the count it matched', () => {
    open({
      record: record({ slash_commands: [{ name: '/clear', description: 'Clear chat history' }] }),
    });
    type('/mod');

    expect(drawn(), 'the header names the list').toContain('commands');
    expect(
      [...document.querySelectorAll('.ac .it')].map((row) => row.textContent?.trim()),
      "forge's table is what it offers, matched on the description too",
    ).toEqual(['/mode Show / set session mode', '/model Show / set session model']);
    expect(document.querySelector('.ac .h .n')?.textContent, 'the header counts the matches').toBe(
      '2',
    );
  });

  it('marks the span the list matched on, and marks nothing when the query is empty', () => {
    open({ record: record({ subagents: [{ name: 'cli-version', description: 'settled 3m' }] }) });
    type('&cli');

    expect(
      document.querySelector('.ac .it .p em')?.textContent,
      'the match is the marked span',
    ).toBe('cli');
  });

  it('writes the picked row into the draft, replacing the token it opened on', () => {
    open({
      record: record({
        slash_commands: [{ name: '/clear', description: 'Clear chat history' }],
      }),
    });
    type('run /mo');
    expect(
      document.querySelector('.ac'),
      'a command is the whole draft while it is typed',
    ).toBeNull();

    type('/mo');
    const rows = [...document.querySelectorAll('.ac .it')];
    const model = rows.find((row) => row.textContent.includes('/model'));
    if (!(model instanceof HTMLElement)) throw new Error('the list offered no /model');
    model.click();
    flushSync();

    expect(field().value, 'the token is replaced and the caret is left to type on').toBe('/model ');
    expect(document.querySelector('.ac'), 'and the list closes behind the pick').toBeNull();
  });

  it('closes the list on escape, and leaves the typed token alone', () => {
    open({
      record: record({
        file_index: {
          entries: {
            'src/home.rs': {
              rel_path: 'src/home.rs',
              rel_path_lower: 'src/home.rs',
              basename_lower: 'home.rs',
              depth: 1,
            },
          },
        },
      }),
    });
    type('@hom');
    expect(document.querySelector('.ac'), 'the @ trigger opens the file list').not.toBeNull();

    press('Escape');

    expect(document.querySelector('.ac'), 'the list goes').toBeNull();
    expect(field().value, 'and what the reader typed stays').toBe('@hom');
  });
});

/**
 * Two clients on one seat, which is the phone and the desktop at once.
 *
 * The core arbitrates: a prompt is drawn from what the core reports and never
 * from a copy of the client's own, so an answer given anywhere is one the other
 * sees leave. Both halves are here - the dock closing on both, and the answer
 * carrying the option the core offered rather than one the client invented.
 */
describe('two clients on one seat', () => {
  /**
   * A landing belongs to the client that took it, so one handed a notice it
   * never took has no draft those words belong to. Landing them there is the
   * reader's own message arriving back in a box they did not type it in.
   */
  it('leaves the box empty on the client that never saw the take', async () => {
    const { one, other } = openBoth({ dictation: true });

    // One client begins the take, and is the only one shown it: the other is
    // handed the landing alone, as a client that attached after the take did.
    await beginTakeOn(0, one);

    const landed = record({
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'push it once CI is green', truncated: false },
        machine: null,
      },
    });
    one.page.record = landed;
    other.page.record = landed;
    flushSync();

    const [watched, missed] = bothFields();
    expect(watched?.value, 'the client that began the take lands it').toBe(
      'push it once CI is green',
    );
    expect(missed?.value, 'the one that attached after it does not').toBe('');
  });

  /**
   * **A landing is this page's to place only when this page began a take, and
   * the arm it sets is spent by the landing it placed.** A take's landing is
   * the seat's, so it reaches every client reading that record: a client that
   * never began the take meets the words with no arm of its own, and one that
   * did begin a take must not ride a later landing with the spent arm (Ved,
   * live, 2026-10-15). Take news itself is unicast to its initiator, so these
   * guards are safety today rather than a live route - they are what the cases
   * here pin.
   */
  it('places a take on the client that began it, and never on the one that watched', async () => {
    const { one, other } = openBoth({ dictation: true });

    // The first client begins a take and abandons it: a cancelled take has no
    // notice coming, so the arm goes with it rather than standing for the next
    // landing - which, here, is another client's words.
    await beginTakeOn(0, one);
    const cancel = document.querySelectorAll('.tc button.x')[0];
    if (!(cancel instanceof HTMLElement)) throw new Error('the take card drew no cancel');
    cancel.click();
    flushSync();

    // The core's own end of an abandoned take: the record keeps nothing of it,
    // and no notice comes of a take nobody submitted.
    const gone = withNotice(null);
    one.page.record = gone;
    other.page.record = gone;
    flushSync();

    // The other client begins its own, and its landing reaches the seat.
    await beginTakeOn(1, other);
    const landed = withNotice(LANDED);
    one.page.record = landed;
    other.page.record = landed;
    flushSync();

    const [afterCancel, owned] = bothFields();
    expect(afterCancel?.value, "the cancelled take's client placed another client's words").toBe(
      '',
    );
    expect(owned?.value, 'the client that began the take places its own words').toBe(LANDED.text);

    // Round trip: the first client's own take lands, which spends its arm, and
    // then the other client's take lands - it must not ride the spent arm. The
    // fields are re-read at each landing: the box is remounted when a take
    // card takes its place, so a handle taken before one is a detached node.
    await beginTakeOn(0, one);
    const own = withNotice({ ...LANDED, text: 'and run the gate too' });
    one.page.record = own;
    other.page.record = own;
    flushSync();

    expect(bothFields()[0]?.value, 'the first client lands its own take').toBe(
      'and run the gate too',
    );

    await beginTakeOn(1, other);
    const theirs = withNotice({ ...LANDED, text: 'a third take' });
    one.page.record = theirs;
    other.page.record = theirs;
    flushSync();

    const [spent, third] = bothFields();
    expect(spent?.value, "a spent arm rode another client's take").toBe('and run the gate too');
    expect(third?.value, 'and the client that began that one places it').toBe(
      `${LANDED.text} a third take`,
    );
  });

  it('draws the same prompt on both, and the answer the core offered is what one sends', () => {
    const { shared, one, other } = openBoth();
    const asked = record({ pending_asks: [permissionAsk()] });
    one.page.record = asked;
    other.page.record = asked;
    flushSync();

    expect(document.querySelectorAll('.acts .btn'), 'both clients draw the prompt').toHaveLength(4);

    action('Allow once').click();
    flushSync();

    expect(shared.sent, "one answer went, and it is the core's own option").toEqual([
      {
        command: {
          respond_permission: {
            key: { org: 'Busytools', project: 'forge', label: 'lead' },
            tool_id: 'tu-1',
            outcome: {
              outcome: 'selected',
              option_id: 'opt-once',
              action: { kind: 'allow' },
            },
          },
        },
      },
    ]);
  });

  it('dismisses the dock on the other client when the core says the prompt is gone', () => {
    const { shared, one, other } = openBoth();
    const asked = record({ pending_asks: [permissionAsk()] });
    one.page.record = asked;
    other.page.record = asked;
    flushSync();

    action('Deny').click();
    flushSync();
    expect(shared.sent, 'the second client answered on its own').toHaveLength(1);

    // The core resolves once, and both pages re-read one prompt, so what the
    // other client draws is the read rather than anything this one told it.
    const gone = record();
    one.page.record = gone;
    other.page.record = gone;
    flushSync();

    expect(document.querySelectorAll('.opt'), 'the dock outlived the prompt').toHaveLength(0);
    expect(field(), 'and the box is back on both').toBeInstanceOf(HTMLTextAreaElement);
  });

  it('says why when the core refuses the answer', () => {
    const { shared, one } = openBoth();
    one.page.record = record({ pending_asks: [permissionAsk()] });
    flushSync();

    action('Allow once').click();
    flushSync();

    // The core dropped the answer - another device got there first - and the
    // prompt is still waiting, so the dock is still up and the refusal has to
    // be somewhere the reader can read it.
    shared.say({
      kind: 'error',
      what: 'dispatch',
      why: 'the prompt was already answered',
    });
    flushSync();

    expect(drawn(), 'the refusal was swallowed').toContain('the prompt was already answered');
  });
});

/**
 * The dictation panel, which the mic opens.
 *
 * The form follows the mockup rather than the view this client replaced: each
 * axis is a short exclusive set, so each is a row of chips. `dictation.test.ts`
 * pins what the axes and the hint ARE; this pins what the panel DOES with them.
 */
describe('the dictation panel', () => {
  /**
   * The axes and the input are the CLIENT's since capture moved here, so these
   * cases drive them through the storage a reload would read and through the
   * browser's own input list - never through the record, which carries the
   * core's set and nothing this page obeys.
   */
  const SEAT = subjectKey({ session: SLOT });

  /** What a previous session left for this seat, which the panel reads back. */
  function remember(axes: DictateAxes, device: string | null = null): void {
    rememberAxes(SEAT, axes);
    rememberDevice(SEAT, device);
  }

  afterEach(() => {
    // jsdom's storage outlives a test, and a stored axis would move the next
    // one's chips.
    rememberAxes(SEAT, null);
    rememberDevice(SEAT, null);
  });

  /** Let a walk of the inputs settle, which is a promise like the open. */
  async function settled(): Promise<void> {
    for (let turn = 0; turn < 4; turn += 1) await Promise.resolve();
    flushSync();
  }

  /** The panel, opened by holding the mic - a press is the trigger now. */
  function opened(over: Partial<ComposerProps> = {}, on?: Wire) {
    const harness = open({ dictation: true, ...over }, on);
    const mic = document.querySelector('.mic');
    if (!(mic instanceof HTMLElement)) throw new Error('the box drew no mic to open with');
    vi.useFakeTimers();
    try {
      mic.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true }));
      vi.advanceTimersByTime(450);
      mic.dispatchEvent(new MouseEvent('pointerup', { bubbles: true }));
      // The click a browser sends after the release, which the swallow eats -
      // leaving it un-sent would linger the flag into a later case.
      mic.click();
    } finally {
      vi.useRealTimers();
    }
    flushSync();
    if (document.querySelector('.pop') === null) throw new Error('the hold drew no panel');
    return harness;
  }

  /** The words an element draws, with the markup's own whitespace collapsed. */
  function words(el: Element | null | undefined): string {
    return (el?.textContent ?? '').replace(/\s+/g, ' ').trim();
  }

  /**
   * Every chip a click can act on, with the axis it belongs to.
   *
   * Buttons only, and that is the assertion rather than an implementation
   * detail: the mode's chips are spans, because nothing in the core can set
   * one, and a mode drawn as a button would be a control nothing honours.
   */
  function chips(): { axis: string; label: string; on: boolean }[] {
    return [...document.querySelectorAll('.pop .ax')].flatMap((group) => {
      const axis = words(group.querySelector('.lbl'));
      return [...group.querySelectorAll('button.chip')].map((chip) => ({
        axis,
        label: words(chip),
        on: chip.classList.contains('on'),
      }));
    });
  }

  it('draws the value in force from what this client holds', () => {
    opened();
    expect(
      chips()
        .filter((chip) => chip.on)
        .map((chip) => chip.label),
      'with nothing stored the crate defaults stand, which is what the panel draws as unset',
    ).toEqual(['semi-formal', 'prose', 'plain text']);

    void unmount(app as Record<string, unknown>);
    app = null;
    document.body.innerHTML = '';
    remember({ ...DEFAULT_AXES, styling: 'casual', structure: 'lists' });
    opened();
    expect(
      chips()
        .filter((chip) => chip.on)
        .map((chip) => chip.label),
      'and a seat this client set is what it comes back to',
    ).toEqual(['casual', 'may bullet a list', 'plain text']);
  });

  /**
   * The greeting is where the defaults come from: `[dictate]`'s keys reach the
   * client beside mark, theme and font, and they are what the panel draws as
   * the unset state and what the reset row returns to.
   */
  it('draws the config defaults the greeting carried', () => {
    opened({}, wire({ ...DEFAULT_SETTINGS, dictate: { ...DEFAULT_AXES, styling: 'formal' } }));

    expect(
      chips()
        .filter((chip) => chip.on)
        .map((chip) => chip.label),
    ).toEqual(['formal', 'prose', 'plain text']);
    expect(
      [...document.querySelectorAll('.pop .lbl .src')].filter(
        (held) => held.textContent?.includes('this session') === true,
      ),
      'a value the config set is not a value this session moved',
    ).toHaveLength(0);
  });

  it('marks an axis this client moved off the config value, and only that one', () => {
    remember({ ...DEFAULT_AXES, styling: 'formal' });
    opened();

    // The mode and the input carry a source tag of their own - they come from
    // elsewhere than the axes - so the tag is read by its words rather than by
    // the class alone.
    const marked = [...document.querySelectorAll('.pop .lbl .src')]
      .filter((held) => held.textContent?.includes('this session') === true)
      .map((held) => words(held.parentElement));
    expect(marked, 'the source tag names the axis this client moved').toEqual([
      'VOICE · this session',
    ]);
  });

  it('sets one axis when a chip is clicked, and remembers it here', () => {
    const harness = opened();

    const chip = [...document.querySelectorAll('.pop .chip')].find(
      (held) => held.textContent?.trim() === 'casual',
    );
    if (!(chip instanceof HTMLElement)) throw new Error('the voice axis drew no casual chip');
    chip.click();
    flushSync();

    expect(
      chips()
        .filter((held) => held.on)
        .map((held) => held.label),
      'the chip is in force at once',
    ).toContain('casual');
    expect(axesFor(SEAT, DEFAULT_AXES).styling, 'and stored, so a reload comes back to it').toBe(
      'casual',
    );
    expect(harness.sent, "an axis is this client's: nothing crosses the socket for it").toEqual([]);
  });

  it('states the mode rather than offering it, because nothing can set it', () => {
    opened({
      record: record({
        composer: { take: null, notice: null, machine: null, mode: 'hold' },
      }),
    });

    const mode = [...document.querySelectorAll('.pop .ax')].find(
      (group) => group.querySelector('.lbl')?.textContent?.includes('MODE') === true,
    );
    if (mode === undefined) throw new Error('the panel drew no mode row');

    expect(
      mode.querySelectorAll('button'),
      'a mode chip would be a control nothing honours',
    ).toHaveLength(0);
    expect(mode.textContent, 'the value in force is stated').toContain('hold');
    expect(mode.textContent, 'with its rule under it').toContain('release transcribes');
    expect(mode.textContent, 'and where it comes from').toContain('forge.toml');
  });

  it('asks this machine for its inputs once, and picks one by its id', async () => {
    const walk = vi.mocked(inputs);
    walk.mockResolvedValueOnce([
      { id: 'mic-2', label: 'Shure SM7B' },
      { id: 'mic-9', label: 'MacBook Pro Microphone' },
    ]);
    const harness = opened();
    expect(document.querySelector('.pop .dev')?.textContent, 'nothing is picked yet').toContain(
      'System default',
    );

    const door = document.querySelector('.pop .dev');
    if (!(door instanceof HTMLElement)) throw new Error('the panel drew no device row');
    door.click();
    await settled();
    expect(walk, "one walk, which is the browser's own list").toHaveBeenCalledTimes(1);

    const row = [...document.querySelectorAll('.pop .drow')].find((held) =>
      held.textContent?.includes('MacBook Pro Microphone'),
    );
    if (!(row instanceof HTMLElement)) throw new Error('the list drew no second device');
    row.click();
    flushSync();

    expect(
      document.querySelector('.pop .dev')?.textContent,
      'the row names the input this seat records from',
    ).toContain('MacBook Pro Microphone');
    expect(deviceFor(SEAT), 'and the pick is remembered by its id, which is the identity').toBe(
      'mic-9',
    );
    expect(harness.sent, "the input is this machine's: nothing crosses the socket for it").toEqual(
      [],
    );
  });

  it('names the system default once a take has opened it, and keeps the list quiet', async () => {
    const walk = vi.mocked(inputs);
    walk.mockResolvedValueOnce([
      { id: 'mic-2', label: 'Shure SM7B' },
      { id: 'mic-9', label: 'MacBook Pro Microphone' },
    ]);
    localStorage.setItem(
      'forge.dictate.default',
      JSON.stringify({ id: 'mic-2', label: 'Shure SM7B' }),
    );
    try {
      opened();
      expect(
        document.querySelector('.pop .dev')?.textContent,
        'the row names what the default resolved to',
      ).toContain('Shure SM7B (system default)');

      const door = document.querySelector('.pop .dev');
      if (!(door instanceof HTMLElement)) throw new Error('the panel drew no device row');
      door.click();
      await settled();

      const device = [...document.querySelectorAll('.pop .ax')].find(
        (group) => group.querySelector('.lbl')?.textContent === 'INPUT DEVICE',
      );
      expect(device?.querySelector('.note'), 'a named list owes no explanation').toBeNull();
    } finally {
      localStorage.removeItem('forge.dictate.default');
    }
  });

  it('explains unnamed rows only while the list has any', async () => {
    const walk = vi.mocked(inputs);
    walk.mockResolvedValueOnce([{ id: 'mic-2', label: '' }]);
    opened();
    const door = document.querySelector('.pop .dev');
    if (!(door instanceof HTMLElement)) throw new Error('the panel drew no device row');
    door.click();
    await settled();

    const device = [...document.querySelectorAll('.pop .ax')].find(
      (group) => group.querySelector('.lbl')?.textContent === 'INPUT DEVICE',
    );
    expect(
      device?.querySelector('.note')?.textContent ?? '',
      'a list the browser would not name says why',
    ).toContain('names appear once this page has been allowed the microphone');
  });

  it('resets every axis and the input back to what the config set', () => {
    remember({ ...DEFAULT_AXES, styling: 'formal', context: 'email' }, 'mic-9');
    const harness = opened();
    expect(
      chips()
        .filter((chip) => chip.on)
        .map((chip) => chip.label),
      'the panel opens on what this client holds',
    ).toEqual(['formal', 'prose', 'email layout']);

    const reset = document.querySelector('.pop .rst');
    if (!(reset instanceof HTMLElement)) throw new Error('the panel drew no reset');
    reset.click();
    flushSync();

    expect(
      chips()
        .filter((chip) => chip.on)
        .map((chip) => chip.label),
      'the reset returns to the config defaults',
    ).toEqual(['semi-formal', 'prose', 'plain text']);
    expect(deviceFor(SEAT), 'and the input pick goes with them').toBeNull();
    expect(harness.sent, 'nothing crosses the socket for a reset either').toEqual([]);
  });

  it('advertises the bound key, and nothing when the binding is off', () => {
    opened({
      record: record({
        composer: { take: null, notice: null, machine: null, bind: 'left_cmd' },
      }),
    });
    expect(document.querySelector('.pop .hd')?.textContent).toContain('left to talk');

    void unmount(app as Record<string, unknown>);
    app = null;
    document.body.innerHTML = '';
    opened({
      record: record({
        composer: { take: null, notice: null, machine: null, bind: 'off' },
      }),
    });
    expect(
      document.querySelector('.pop .hd')?.textContent,
      'a hint naming a key that will never fire is worse than none',
    ).not.toContain('to talk');
  });

  it('closes on Escape, and gives the keyboard back to the field', () => {
    opened();
    expect(document.querySelector('.pop')).not.toBeNull();

    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    flushSync();

    expect(document.querySelector('.pop'), 'the panel takes the Escape it is open for').toBeNull();
    expect(document.activeElement, 'and the reader is typing again').toBe(field());
  });

  it('hides on its own close, which is the way out a touch screen has', () => {
    opened();

    const x = document.querySelector('.pop .hd .x');
    if (!(x instanceof HTMLElement)) throw new Error('the panel drew no close');
    // The browser focuses what it activates: jsdom's click does not, and
    // without this the mount's own field focus answers the assertion below.
    x.focus();
    x.click();
    flushSync();

    expect(document.querySelector('.pop'), 'the close is the door out').toBeNull();
    expect(document.activeElement, 'the keyboard returns to the field').toBe(field());
  });

  /**
   * A walk that failed. The socket's own contract is explicit - the refusal is
   * rendered where the list would have been - so an empty list region and a
   * failed walk must not draw the same. This is the composer's own pattern for
   * a refused dispatch, one file over.
   */
  it('draws a failed walk in the list region rather than as an empty list', async () => {
    vi.mocked(inputs).mockRejectedValueOnce(new Error('nope'));
    opened();

    const door = document.querySelector('.pop .dev');
    if (!(door instanceof HTMLElement)) throw new Error('the panel drew no device row');
    door.click();
    await settled();

    const region = document.querySelector('.pop .list');
    expect(region?.textContent, 'the refusal is drawn where the list would be').toContain(
      'the inputs could not be listed',
    );
    expect(
      region?.textContent,
      'and a failed walk does not read as a walk that found nothing',
    ).not.toContain('No input devices found');
  });

  it('walks once however many times the row is clicked, and closes on the next', async () => {
    const walk = vi.mocked(inputs);
    walk.mockResolvedValue([{ id: 'mic-9', label: 'MacBook Pro Microphone' }]);
    opened();
    const door = document.querySelector('.pop .dev');
    if (!(door instanceof HTMLElement)) throw new Error('the panel drew no device row');

    // Three clicks before any answer: the walk asks the browser for its
    // devices, so only the first may leave the panel.
    door.click();
    door.click();
    door.click();
    await settled();
    expect(walk, 'the walk is the expensive part, so it is asked for once').toHaveBeenCalledTimes(
      1,
    );

    const again = document.querySelector('.pop .dev');
    if (!(again instanceof HTMLElement)) throw new Error('the row went with the list');
    again.click();
    flushSync();
    expect(document.querySelector('.pop .list'), 'a click collapses it again').toBeNull();

    again.click();
    await settled();
    expect(walk, 'and reopening asks for nothing a second time').toHaveBeenCalledTimes(1);
  });

  it('marks an input that is not there any more', async () => {
    remember(DEFAULT_AXES, 'walked-off-2');
    vi.mocked(inputs).mockResolvedValueOnce([{ id: 'mic-9', label: 'MacBook Pro Microphone' }]);
    opened();

    const door = document.querySelector('.pop .dev');
    if (!(door instanceof HTMLElement)) throw new Error('the panel drew no device row');
    door.click();
    await settled();

    const row = document.querySelector('.pop .dev');
    expect(row?.textContent, 'the absent input is named as absent').toContain('not present');
    expect(row?.classList.contains('missing'), 'and it is marked, not only worded').toBe(true);
  });

  /**
   * A measured rect on an element jsdom lays out nothing for.
   *
   * `clientTop`/`clientHeight` come with it because the panel hangs off the
   * host's padding box, which is what the CSS anchor measured against: a
   * borderless host, as every one of these is, has its border at zero and its
   * padding box the whole rect.
   */
  function rectAt(el: Element, top: number, bottom: number): void {
    Object.defineProperty(el, 'getBoundingClientRect', {
      configurable: true,
      value: () => ({
        top,
        bottom,
        height: bottom - top,
        left: 0,
        right: 0,
        width: 0,
        x: 0,
        y: top,
        toJSON: () => ({}),
      }),
    });
    Object.defineProperty(el, 'clientTop', { configurable: true, value: 0 });
    Object.defineProperty(el, 'clientHeight', { configurable: true, value: bottom - top });
  }

  /**
   * The cap follows the INPUT ROW, not the whole surface above it.
   *
   * A list opening above the field, or a take row landing above it, grows the
   * input row and lifts the panel with it - and a cap measured on mount would
   * stay where it was and cut the panel's own top off. jsdom has no
   * `ResizeObserver` and performs no layout, so what this pins is that one is
   * watching the row; that the cap then tracks a growing row is a browser
   * measurement, and the body carries its numbers.
   */
  it('watches the input row, so a box that grows under the panel re-measures it', () => {
    const observed: Element[] = [];
    const original: unknown = Reflect.get(globalThis, 'ResizeObserver');
    class Watching {
      run: () => void;
      constructor(run: () => void) {
        this.run = run;
      }
      observe(el: Element): void {
        observed.push(el);
        this.run();
      }
      disconnect(): void {}
    }
    Reflect.set(globalThis, 'ResizeObserver', Watching);
    try {
      opened();

      expect(
        observed.length,
        'nothing watches the input row, so a box growing under the panel re-clips it',
      ).toBeGreaterThan(0);
      expect(observed[0]?.classList.contains('line'), 'the input row is what is watched').toBe(
        true,
      );
    } finally {
      Reflect.set(globalThis, 'ResizeObserver', original);
    }
  });

  /**
   * And the cap READS what the row reports.
   *
   * The assertion above pins that something watches; this pins that the watch
   * does something - the regression the whole cap exists for. jsdom performs no
   * layout, but it does not have to: the rects are supplied as own properties on
   * the composer's own elements, so the component's calls answer what a browser
   * would have measured, and the callback a fake observer was handed is fired by
   * hand.
   */
  it('caps the panel from the input row it hangs off', () => {
    const callbacks: (() => void)[] = [];
    const original: unknown = Reflect.get(globalThis, 'ResizeObserver');
    class Watching {
      run: () => void;
      constructor(run: () => void) {
        this.run = run;
      }
      observe(): void {
        callbacks.push(this.run);
      }
      disconnect(): void {}
    }
    Reflect.set(globalThis, 'ResizeObserver', Watching);
    try {
      opened();

      const pop = document.querySelector('.pop');
      const comp = document.querySelector('.comp');
      const box = document.querySelector('.comp .line');
      if (!(pop instanceof HTMLElement)) throw new Error('the panel drew nothing to cap');
      if (!(comp instanceof Element)) throw new Error('the composer drew no surface');
      if (!(box instanceof Element)) throw new Error('the composer drew no input row');

      rectAt(comp, 0, 500);
      rectAt(box, 380, 425);
      const fire = callbacks.at(-1);
      if (fire === undefined) throw new Error('nothing watched the input row');
      fire();
      flushSync();

      expect(pop.style.maxHeight, "the cap reads the row's top").toBe('368px');
      expect(pop.style.bottom, 'and the hang is held from the page foot').toBe('126px');
    } finally {
      Reflect.set(globalThis, 'ResizeObserver', original);
    }
  });

  /**
   * **A page update is not a new read.**
   *
   * The models subscription used to be made inside the effect that watches the
   * connection, and the page hands the panel its props again on every record
   * frame - so each one tore the subscription down and built it again. The
   * store goes with its last reader, so the subscribe that followed found no
   * snapshot, asked the server, and the panel drew "reading the models this
   * session runs..." for the round trip: measured in the dev client, a
   * one-frame flicker of the panel's model rows on every update (#2524). One
   * connection, one subscription.
   */
  it('reads the models once per connection, not once per page update', () => {
    const shared = wire();
    const asked: Subject[] = [];
    const subscribe = shared.connection.subscribe.bind(shared.connection);
    shared.connection.subscribe = (what) => {
      asked.push(what);
      return subscribe(what);
    };

    const harness = opened({}, shared);
    const models = (): number => asked.filter((what) => what === 'dictate_models').length;
    expect(models(), 'the panel asked for the models it draws').toBe(1);

    // The page hands the composer a new record, as a frame landing does.
    harness.page.record = record({ header: { turn_in_flight: true } });
    flushSync();

    expect(document.querySelector('.pop'), 'the panel closed on the page update').not.toBeNull();
    expect(models(), 'the page update tore the read down and asked again').toBe(1);

    // The read the panel kept is still LIVE: a landing on the subject redraws
    // its rows rather than leaving a subscription nobody hears.
    harness.say({ kind: 'update', update: { dictate_models_changed: { models: modelsWire } } });
    flushSync();
    expect(document.body.textContent, 'the models landing never reached the panel').toContain(
      'cohere-transcribe-03-2026-Q4_K_M.gguf',
    );
  });

  /**
   * **A turn's strip lane is above the input row, and the panel hangs off the
   * row, so the strip's comings and goings cannot reach it.**
   *
   * The surface carries the pinned strip, whose height moves with the turn: the
   * figures row appears at the start, the chips re-wrap, the row goes at the
   * end. Measured in the dev client (#2524), a panel anchored to the surface
   * jumped to a negative top - its header clipped - for one frame at every turn
   * start and dropped 25px for one at every turn end, and its cap shrank by the
   * strip's own height. Anchored to the input row, the strip growing 25px
   * upward moves the input row not at all, and the panel with it.
   */
  it("keeps its place while a turn's strip grows above the input row", () => {
    const callbacks: (() => void)[] = [];
    const original: unknown = Reflect.get(globalThis, 'ResizeObserver');
    class Watching {
      run: () => void;
      constructor(run: () => void) {
        this.run = run;
      }
      observe(): void {
        callbacks.push(this.run);
      }
      disconnect(): void {}
    }
    Reflect.set(globalThis, 'ResizeObserver', Watching);
    try {
      opened();

      const pop = document.querySelector('.pop');
      const comp = document.querySelector('.comp');
      const box = document.querySelector('.comp .line');
      if (!(pop instanceof HTMLElement)) throw new Error('the panel drew nothing to place');
      if (!(comp instanceof Element)) throw new Error('the composer drew no surface');
      if (!(box instanceof Element)) throw new Error('the composer drew no input row');

      // At rest on the page: the strip lane 74px deep, the input row 45px, the
      // page's foot at 608.
      rectAt(comp, 488, 608);
      rectAt(box, 562, 607);
      const fire = callbacks.at(-1);
      if (fire === undefined) throw new Error('nothing watched the panel');
      fire();
      flushSync();

      expect(pop.style.bottom, 'the panel hangs 6px above the input row').toBe('52px');
      expect(pop.style.maxHeight, 'capped by the room above the input row').toBe('550px');

      // A turn starts: the strip lane gains its figures row and the surface
      // grows 25px UPWARD. The input row and the page's foot hold still.
      rectAt(comp, 463, 608);
      fire();
      flushSync();

      expect(document.querySelector('.pop'), 'the panel closed on the turn update').toBe(pop);
      expect(pop.style.bottom, 'the turn moved the panel off its row').toBe('52px');
      expect(pop.style.maxHeight, 'the turn re-capped the panel').toBe('550px');
    } finally {
      Reflect.set(globalThis, 'ResizeObserver', original);
    }
  });

  /**
   * **A prompt's dock taking the box is not the panel's row leaving for good.**
   *
   * The composer swaps `.box` for the dock when an ask lands, so the row the
   * panel hangs off is detached. A detached row's ResizeObserver fires once
   * more at 0x0, and measuring that wrote `bottom: hostBottom + 6;
   * max-height: 0`: the open panel collapsed under a prompt that had nothing
   * to do with it. The observation stops with the row and the place measured
   * last stands; the box that replaces the dock re-measures.
   */
  it('keeps its place when a dock takes the box', () => {
    const callbacks: (() => void)[] = [];
    const original: unknown = Reflect.get(globalThis, 'ResizeObserver');
    class Watching {
      run: () => void;
      dead = false;
      constructor(run: () => void) {
        this.run = run;
      }
      observe(): void {
        callbacks.push(() => {
          if (!this.dead) this.run();
        });
      }
      disconnect(): void {
        this.dead = true;
      }
    }
    Reflect.set(globalThis, 'ResizeObserver', Watching);
    try {
      const harness = opened();

      const pop = document.querySelector('.pop');
      const comp = document.querySelector('.comp');
      const box = document.querySelector('.comp .line');
      if (!(pop instanceof HTMLElement)) throw new Error('the panel drew nothing to place');
      if (!(comp instanceof Element)) throw new Error('the composer drew no surface');
      if (!(box instanceof Element)) throw new Error('the composer drew no input row');

      rectAt(comp, 488, 608);
      // A row that is detached measures 0x0, which is what the browser does.
      Object.defineProperty(box, 'getBoundingClientRect', {
        configurable: true,
        value: () =>
          box.isConnected
            ? {
                top: 562,
                bottom: 607,
                height: 45,
                left: 0,
                right: 0,
                width: 0,
                x: 0,
                y: 562,
                toJSON: () => ({}),
              }
            : {
                top: 0,
                bottom: 0,
                height: 0,
                left: 0,
                right: 0,
                width: 0,
                x: 0,
                y: 0,
                toJSON: () => ({}),
              },
      });
      const fire = callbacks.at(-1);
      if (fire === undefined) throw new Error('nothing watched the panel');
      fire();
      flushSync();
      expect(pop.style.bottom, 'the panel hangs 6px above the input row').toBe('52px');
      expect(pop.style.maxHeight, 'capped by the room above the input row').toBe('550px');

      // An ask lands: the dock takes the box's place and the row detaches.
      harness.page.record = record({ pending_asks: [permissionAsk()] });
      flushSync();
      expect(document.querySelector('.comp .line'), 'the dock did not take the box').toBeNull();
      expect(box.isConnected, 'the row is still attached').toBe(false);

      // The detached row's observer fires once more, as the browser does.
      for (const callback of callbacks) callback();
      flushSync();

      expect(document.querySelector('.pop'), 'the panel closed under the dock').not.toBeNull();
      expect(pop.style.bottom, 'the detached row re-placed the panel').toBe('52px');
      expect(pop.style.maxHeight, 'the detached row collapsed the panel').toBe('550px');

      // The ask clears and the composer's own box returns: a NEW row, which
      // must be measured rather than inheriting the place the old one left.
      harness.page.record = record();
      flushSync();
      const again = document.querySelector('.comp .line');
      if (!(again instanceof Element)) throw new Error('the box did not come back');
      rectAt(again, 550, 607);
      const rebind = callbacks.at(-1);
      if (rebind === undefined) throw new Error('nothing watched the row that returned');
      rebind();
      flushSync();
      expect(pop.style.bottom, 'the row that returned did not re-measure the panel').toBe('64px');
      expect(pop.style.maxHeight, 'the room was never taken from the new row').toBe('538px');
    } finally {
      Reflect.set(globalThis, 'ResizeObserver', original);
    }
  });
});

/**
 * The dock's settled shape (2026-10-10): the queue leads, the last row is the
 * input, and the bar folds the question to itself.
 */
describe("the dock's settled shape", () => {
  /** Every command the composer sent, in order. */
  const commands = (harness: { sent: { command: Record<string, unknown> }[] }) =>
    harness.sent.map((entry) => entry.command);

  it('types through from an option: a printable character moves into the words and lands there', () => {
    // The ARIA practice's rule for a list beside an input, and the defect the
    // round was filed for: a reader who starts typing must not have to
    // discover the row first.
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    press('l');

    const notes = document.querySelector('.dock .notes');
    if (!(notes instanceof HTMLTextAreaElement)) throw new Error('no words row');
    expect(document.activeElement, 'the character did not take the caret into the words').toBe(
      notes,
    );
    expect(notes.value, 'and did not land there').toBe('l');
    expect(commands(harness), 'typing answers nothing').toEqual([]);
  });

  it('leads the dock with the waiting count', () => {
    open({
      record: record({ pending_asks: [questionAsk()] }),
      seat: seatRead({ pendingDepth: 3 }),
    });

    const queue = document.querySelector('.dock .queue');
    expect(queue, 'the dock drew no queue line').not.toBeNull();
    const head = document.querySelector('.dock .d-head');
    if (!(queue instanceof HTMLElement) || head === null) return;
    expect(
      queue.compareDocumentPosition(head) & Node.DOCUMENT_POSITION_FOLLOWING,
      'the count does not lead the question',
    ).not.toBe(0);
    expect(queue.textContent, 'the count is not the one waiting behind').toContain(
      '2 more pending',
    );
  });

  it('folds the question to its bar, and the bar unwraps it', () => {
    open({
      record: record({ pending_asks: [questionAsk()] }),
      seat: seatRead({ pendingDepth: 2 }),
    });

    const chev = document.querySelector('.dock .chevb');
    if (!(chev instanceof HTMLElement)) throw new Error('the head drew no fold control');
    chev.click();
    flushSync();

    expect(document.querySelector('.dock .qbar'), 'no bar after folding').not.toBeNull();
    expect(document.querySelector('.dock .opts'), 'the options stayed under the bar').toBeNull();
    expect(document.querySelector('.dock .keys'), 'the keys stayed under the bar').toBeNull();
    expect(document.querySelector('.dock .qbar .cnt')?.textContent, 'the count left the bar').toBe(
      '+1',
    );
    expect(
      document.querySelector('.dock .qbar .chevc'),
      'the bar carries its own unfold glyph',
    ).not.toBeNull();
    expect(
      document.querySelector('.dock .queue'),
      'the pending line drew under the fold, where the bar carries the count instead',
    ).toBeNull();

    const bar = document.querySelector('.dock .qbar');
    if (!(bar instanceof HTMLElement)) throw new Error('no bar');
    bar.click();
    flushSync();

    expect(document.querySelector('.dock .qbar'), 'the bar did not unwrap').toBeNull();
    expect(document.querySelector('.dock .opts'), 'the options did not come back').not.toBeNull();
    expect(
      document.querySelector('.dock .queue'),
      'the pending line did not come back with the body',
    ).not.toBeNull();
  });

  it('folds every kind to its bar, and the bar unwraps it', () => {
    // **The body is one slot, so any ask gives it back.** A question, a
    // permission, a held post, a document hand-off and a browser hand-off all
    // fold to one line carrying the kind's mark and its subject.
    const cases = [
      ['question', questionAsk(), 'Pick the environments to deploy to.'],
      ['permission', permissionAsk(), 'Bash'],
      ['slack_draft', slackDraftAsk(), 'Trust Machines'],
      ['document_hand_off', documentHandOffAsk(), '/proj/docs/plan.md'],
      ['browser_hand_off', browserHandOffAsk(), 'browser hand-off'],
    ] as const;
    for (const [kind, ask, subject] of cases) {
      open({ record: record({ pending_asks: [ask] }) });
      const chev = document.querySelector<HTMLElement>('.dock .chevb');
      if (chev === null) throw new Error(`${kind} drew no fold control`);
      chev.click();
      flushSync();

      const bar = document.querySelector<HTMLElement>('.dock .qbar');
      expect(bar, `${kind} did not fold`).not.toBeNull();
      expect(bar?.querySelector('.ic'), `${kind}'s bar carries no mark`).not.toBeNull();
      expect(bar?.textContent, `${kind}'s bar lost its subject`).toContain(subject);

      bar?.click();
      flushSync();
      expect(document.querySelector('.dock .qbar'), `${kind} did not unwrap`).toBeNull();

      void unmount(app as Record<string, unknown>);
      app = null;
      document.body.innerHTML = '';
    }
  });

  it('keeps a refusal on the folded bar', () => {
    const harness = open({
      record: record({ pending_asks: [questionAsk('tu-q', { multi_select: false })] }),
    });
    options()[1]?.click();
    flushSync();
    harness.say({ kind: 'error', what: 'dispatch', why: 'the prompt was already answered' });
    flushSync();
    expect(
      document.querySelector('.dock .refusal'),
      'precondition: the refusal drew',
    ).not.toBeNull();

    const chev = document.querySelector<HTMLElement>('.dock .chevb');
    if (chev === null) throw new Error('the head drew no fold control');
    chev.click();
    flushSync();

    expect(document.querySelector('.dock .qbar'), 'precondition: folded').not.toBeNull();
    expect(
      document.querySelector('.dock .refusal')?.textContent,
      'the fold hid the reason the answer was refused',
    ).toContain('already answered');
  });

  it("swaps the ask for the reader's draft and back, keeping what they chose", () => {
    const harness = open();
    type('a draft');
    // The ask lands on a box that already holds words: the draft is the
    // reader's, and the ask takes the body.
    harness.page.record = record({ pending_asks: [questionAsk()] });
    flushSync();

    // A choice made in the ask before the swap: one row toggled, another marked.
    press(' ');
    press('ArrowDown');
    flushSync();
    const markedAt = () =>
      document.querySelector('.dock .opt[aria-selected="true"] .lbl')?.textContent;
    expect(
      document.querySelectorAll('.dock .box2.on'),
      'precondition: one row toggled',
    ).toHaveLength(1);
    expect(markedAt(), 'precondition: the second row is marked').toBe('Production');

    const chip = document.querySelector<HTMLElement>('[data-draft]');
    if (chip === null) throw new Error('the row drew no draft chip');
    expect(chip.textContent, 'the chip says how many lines').toContain('draft 1 line');
    chip.click();
    flushSync();

    expect(document.querySelector('.ask-body.covered'), 'the ask was not hidden').not.toBeNull();
    const field = document.querySelector<HTMLTextAreaElement>('.comp .line textarea');
    expect(field?.value, "the reader's draft did not come back").toBe('a draft');

    const back = document.querySelector<HTMLElement>('.live-row .live-door.att');
    if (back === null) throw new Error('the row drew no ask chip to go back with');
    back.click();
    flushSync();

    expect(document.querySelector('.ask-body.covered'), 'the ask did not come back').toBeNull();
    expect(
      document.querySelectorAll('.dock .box2.on'),
      'the toggled set was lost to the swap',
    ).toHaveLength(1);
    expect(markedAt(), 'the marked row was lost to the swap').toBe('Production');
  });

  it('keeps the bar folded across a repaint of the same prompt', () => {
    // The record is replaced on every frame and on the session poll; the fold
    // is the reader's, keyed on the prompt, so a repaint must not re-expand it.
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    const chev = document.querySelector('.dock .chevb');
    if (!(chev instanceof HTMLElement)) throw new Error('the head drew no fold control');
    chev.click();
    flushSync();
    expect(document.querySelector('.dock .qbar'), 'precondition: folded').not.toBeNull();

    harness.page.record = record({ pending_asks: [questionAsk()] });
    flushSync();

    expect(document.querySelector('.dock .qbar'), 'a repaint re-expanded the bar').not.toBeNull();
  });

  it('swaps the keys line while the caret is in the words row', () => {
    open({ record: record({ pending_asks: [questionAsk()] }) });

    press('ArrowDown');
    press('ArrowDown');

    const keys = document.querySelector('.dock .keys')?.textContent ?? '';
    expect(keys, 'the keys line still reads as the list').toContain('send these words');
    expect(keys, 'and still promises submit').not.toContain('move');
  });

  it('hands the keyboard back when the prompt vanishes under the caret', () => {
    // Review Focus: answered from another view, refused, or replaced - the
    // caret must land somewhere real, never on the body.
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    press('ArrowDown');
    press('ArrowDown');
    const notes = document.querySelector('.dock .notes');
    expect(document.activeElement, 'precondition: the caret is in the words').toBe(notes);

    harness.page.record = record({ pending_asks: [] });
    flushSync();

    expect(document.querySelector('.dock'), 'the dock is gone').toBeNull();
    expect(document.activeElement, 'the caret fell to the body').not.toBe(document.body);
    expect(document.activeElement?.tagName, 'the caret did not land on the composer field').toBe(
      'TEXTAREA',
    );
  });

  it('draws no prompt for the seat being left', () => {
    // **The interregnum.** The page moves seats and hands over the new seat's
    // record only when its read lands; between the two, the record in hand is
    // the seat being LEFT's - and `owns` is the only thing stopping its prompt
    // drawing on the surface moved to.
    const other: SessionSlot = { org: 'Busytools', project: 'forge', label: 'worker' };
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });
    expect(document.querySelector('.dock'), 'precondition: the prompt draws').not.toBeNull();

    harness.page.slot = other;
    flushSync();
    expect(
      document.querySelector('.dock'),
      "the left seat's prompt drew on the seat moved to",
    ).toBeNull();

    harness.page.record = record({ slot: other, pending_asks: [questionAsk()] });
    flushSync();
    expect(
      document.querySelector('.dock'),
      "the moved-to seat's own prompt does not draw",
    ).not.toBeNull();
  });

  it('keeps the caret where the reader put it across a repaint', () => {
    // The docking effect is keyed on the prompt's identity, not the record
    // object, so a repaint of the SAME prompt must not hand the keyboard back
    // to the option list mid-sentence.
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    press('ArrowDown');
    press('ArrowDown');
    const notes = document.querySelector('.dock .notes');
    expect(document.activeElement, 'precondition: the caret is in the words row').toBe(notes);

    harness.page.record = record({ pending_asks: [questionAsk()] });
    flushSync();

    expect(document.activeElement, 'a repaint stole the caret back to the options').toBe(notes);
  });

  it('keeps a focused action across a repaint, which is what the keying is for', () => {
    // **The keying, discriminated.** The caret test above passes even if the
    // dock remounts and something re-focuses the list on arrival; an action
    // button has no re-focus to ride - a remount drops it to the body, so
    // this fails the moment the `{#key}` stops keying on the prompt's own
    // identity.
    const harness = open({ record: record({ pending_asks: [permissionAsk()] }) });

    const btn = document.querySelector('.dock .acts .btn');
    if (!(btn instanceof HTMLButtonElement)) throw new Error('the dock drew no actions');
    btn.focus();
    flushSync();
    expect(document.activeElement, 'precondition: an action holds the keyboard').toBe(btn);

    harness.page.record = record({ pending_asks: [permissionAsk()] });
    flushSync();

    expect(
      document.activeElement,
      'a repaint remounted the dock and dropped the focused action to the body',
    ).toBe(btn);
  });

  it('unfolds by keyboard without answering the marked row', () => {
    // **The accidental-answer defect**: the bar used to be a div with its own
    // key handler, so the Enter that unfolded ALSO reached the dock's handler
    // and answered the marked option. As a real button, the root's guard hands
    // it its keys.
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    const chev = document.querySelector('.dock .chevb');
    if (!(chev instanceof HTMLElement)) throw new Error('the head drew no fold control');
    chev.click();
    flushSync();
    const bar = document.querySelector('.dock .qbar');
    if (!(bar instanceof HTMLButtonElement)) throw new Error('the bar is not a button');
    expect(document.activeElement, 'the fold did not hand the keyboard to the bar').toBe(bar);

    bar.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    // Space is the browser's other activation key for a button, and it must
    // ride the same guard: reaching the dock's handler would toggle the marked
    // row on a question behind the fold, where nobody can see it.
    bar.dispatchEvent(new KeyboardEvent('keydown', { key: ' ', bubbles: true }));
    flushSync();
    expect(commands(harness), 'Space answered the question').toEqual([]);
    expect(
      document.querySelector('.dock .qbar'),
      'Space unfolded behind the guard, or dropped the fold',
    ).not.toBeNull();

    bar.click();
    flushSync();

    expect(commands(harness), 'unfolding answered the question').toEqual([]);
    expect(document.querySelector('.dock .opts'), 'the body did not unfold').not.toBeNull();
    expect(
      [...document.querySelectorAll('.dock .box2')].map((box) => box.classList.contains('on')),
      'a Space rode the guard through and flipped a mark that was never seen',
    ).toEqual([false, false]);
  });
});

/**
 * One shape for every kind, approving first (Ved, 2026-10-10): a permission's
 * actions and a held post's verbs lead with the action that carries the
 * reader's intent forward, and the refusal sits last.
 */
describe('the approving action leads', () => {
  it("orders a permission's actions allow-always, allow-once, deny", () => {
    open({
      record: record({
        pending_asks: [
          permissionAsk('tu-1', {
            options: [
              {
                option_id: 'opt-once',
                name: 'Allow once',
                kind: 'allow',
                action: { kind: 'allow' },
              },
              { option_id: 'opt-deny', name: 'Deny', kind: 'deny', action: { kind: 'deny' } },
              {
                option_id: 'opt-always',
                name: 'Allow always',
                kind: 'allow',
                action: { kind: 'allow' },
              },
              {
                option_id: 'opt-notes',
                name: 'Tell the agent something else',
                kind: 'notes',
                action: { kind: 'deny' },
              },
            ],
          }),
        ],
      }),
    });

    const buttons = [...document.querySelectorAll('.dock .acts .btn')];
    const labels = buttons.map((button) => button.textContent?.trim());
    expect(labels, 'the approving action does not lead').toEqual([
      'Allow always',
      'Allow once',
      'Deny',
    ]);
    expect(
      buttons[0]?.classList.contains('p'),
      'the action that leads is not the one drawn primary',
    ).toBe(true);
  });

  it("orders a held post's verbs Post, then Don't send", () => {
    open({ record: record({ pending_asks: [slackDraftAsk()] }) });

    const labels = [...document.querySelectorAll('.dock .acts .btn')].map((button) =>
      button.textContent?.trim(),
    );
    expect(labels, 'the approved action does not lead').toEqual(['Post', "Don't send"]);
  });

  it("lands the caret in a permission's words row on a typed character, riding the deny", () => {
    open({ record: record({ pending_asks: [permissionAsk()] }) });

    press('n');

    const notes = document.querySelector('.dock .notes');
    expect(document.activeElement, "typing did not land in the permission's words row").toBe(notes);
    expect(
      notes instanceof HTMLTextAreaElement ? notes.value : null,
      'and did not land there',
    ).toBe('n');
    const deny = [...document.querySelectorAll('.dock .acts .btn')].find((button) =>
      button.textContent?.includes('Deny'),
    );
    expect(deny?.textContent, 'the deny does not say the words ride it').toContain(
      'with these words',
    );
  });
});
