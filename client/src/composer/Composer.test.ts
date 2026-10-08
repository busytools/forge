// @vitest-environment jsdom
import { readFileSync } from 'node:fs';
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { hideBrowser, showBrowser } from '../browser/host';

/**
 * The shell's own door, mock-able so both halves of Open's claim are testable
 * here: outside the shell the real one answers a reason, which only ever
 * proved the could-not line.
 */
vi.mock('../browser/host', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../browser/host')>();
  return {
    ...actual,
    // No window in tests: the shell's raise answers the reason it could not
    // land, or null when it did. The default takes it as landing; a case
    // that wants the failure resolves the reason instead.
    showBrowser: vi.fn(() => Promise.resolve(null)),
    hideBrowser: vi.fn(() => Promise.resolve()),
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
    gated: boolean;
    release: (() => void) | null;
    resolved: { id: string; label: string } | null;
  } = { onFrame: null, stops: 0, gated: false, release: null, resolved: null };
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
    stop: (): void => {
      held.stops += 1;
    },
  };
  return {
    held,
    /** Open at once, or wait for `held.release()` while gated. */
    open: (): Promise<unknown> =>
      held.gated
        ? new Promise((resolve) => {
            held.release = () => resolve(source);
          })
        : Promise.resolve(source),
  };
});

vi.mock('./mic', () => ({
  Microphone: { open: mic.open },
  // A walk a case drives: `mockResolvedValueOnce`/`mockRejectedValueOnce`.
  inputs: vi.fn((): Promise<unknown[]> => Promise.resolve([])),
  inputLine: (): string => 'the inputs could not be listed \u{b7} try again',
}));

import Harness from './Harness.svelte';
import { encodeFrame } from './capture.svelte';
import { echoes, type Echo } from '../chat/echoes.svelte';
import { boxKey } from './box.svelte';
import { subjectKey, type ServerMessage } from '../protocol';
import { DEFAULT_AXES, type DictateAxes } from '../session/wire';
import {
  browserHandOffAsk,
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
import { TRUNCATED, type ComposerProps, type ComposerRecord, type SeatRead } from './view';

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

let app: Record<string, unknown> | null = null;
let second: Record<string, unknown> | null = null;

afterEach(() => {
  if (app !== null) void unmount(app);
  if (second !== null) void unmount(second);
  app = null;
  second = null;
  vi.mocked(hideBrowser).mockClear();
  document.body.innerHTML = '';
  // The pending send outlives the page that drew it, so a case that leaves one
  // behind would hand it to the next.
  echoes.clear(boxKey(SLOT));
  echoes.clear(boxKey(ELSEWHERE));
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
  return record({ slot, composer: { take: held, notice, compacting: false, sign_in: null } });
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

/** A take the box has seen, which is what arms the landing. */
function seesTake(harness: { page: { record: ComposerRecord } }): void {
  harness.page.record = withNotice(null, take());
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

/**
 * The one behaviour here whose failure destroys something a person typed.
 *
 * The box morphs into the dock, and the draft is the client's own state rather
 * than the field's, so the box coming back is the same person's words. This is
 * the input-loss defect the project already has an incident for, which is why
 * it is a test rather than an intention.
 */
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
    shell.className = 'app';
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

  it('draws the card on the box while a take runs, and takes it away when it resolves', () => {
    const harness = open();

    harness.page.record = record({
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
    });
    flushSync();

    const card = document.querySelector('.box .tc');
    expect(card, 'the card is drawn inside the box').not.toBeNull();
    expect(card?.previousElementSibling, 'as its first row, above the draft').toBeNull();

    harness.page.record = record();
    flushSync();

    expect(document.querySelector('.tc'), 'the card goes with the take').toBeNull();
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

  it("lands a take's words at the caret and takes one green beat before easing back", () => {
    vi.useFakeTimers();
    try {
      const harness = open();
      type('fix the');

      // The take has to be one this composer watched: the words a notice
      // carries are the take's, and only a take this client saw end is its
      // own to put in the box.
      harness.page.record = record({
        composer: { take: take(), notice: null, compacting: false, sign_in: null },
      });
      flushSync();

      harness.page.record = record({
        composer: {
          take: null,
          notice: { kind: 'landed', text: 'flaky retry test', truncated: false },
          compacting: false,
          sign_in: null,
        },
      });
      flushSync();

      expect(field().value, 'the words land where the reader was about to type').toBe(
        'fix the flaky retry test',
      );
      expect(document.querySelector('.box')?.classList.contains('done'), 'one green beat').toBe(
        true,
      );

      vi.advanceTimersByTime(1000);
      flushSync();

      expect(
        document.querySelector('.box')?.classList.contains('done'),
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
  it('moves the keyboard to the box a take landed in, so an immediate Enter sends', () => {
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

    harness.page.record = record({
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
    });
    flushSync();
    harness.page.record = record({
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'push it once CI is green', truncated: false },
        compacting: false,
        sign_in: null,
      },
    });
    flushSync();

    expect(
      document.activeElement,
      'and the take brings it back, so an immediate Enter submits',
    ).toBe(field());
  });

  /**
   * The server holds a landed notice until the next take starts, so a client
   * that attaches - or reloads - finds words whose take it never saw. They are
   * the reader's own, already sent, and the box they come back in is not a
   * draft: it is the message they wrote, handed to them to send twice.
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
          compacting: false,
          sign_in: null,
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
          compacting: false,
          sign_in: null,
        },
      }),
    });

    expect(field().value, 'the words did not land here').toBe('');
    expect(document.querySelector('.notice.warn'), 'and nothing says they were cut').toBeNull();
  });

  it("lands a truncated take's words and still says the take was cut", () => {
    const harness = open();
    harness.page.record = record({
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
    });
    flushSync();

    harness.page.record = record({
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'this is what fitted', truncated: true },
        compacting: false,
        sign_in: null,
      },
    });
    flushSync();

    expect(field().value, 'the words land').toBe('this is what fitted');
    expect(
      document.querySelector('.notice.warn')?.textContent,
      'and the row says the take was cut rather than that they stopped speaking',
    ).toBe(TRUNCATED);
  });

  /**
   * The dock takes the box while a prompt waits, and it says what happens to a
   * take that lands behind it. That is the path the notice exists for, and the
   * words still have to be there when the box comes back.
   */
  it('holds a take that lands while a prompt has the box, and lands its words when the box returns', () => {
    const harness = open();
    type('ship it once CI is green');

    // **The take starts in the composer's own box** - that is the moment its
    // destination is captured - and the prompt arrives mid-flight, which does
    // not take words that were spoken for the draft the reader was in.
    harness.page.record = record({
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
    });
    flushSync();

    const watching = record({
      pending_asks: [permissionAsk()],
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
    });
    harness.page.record = watching;
    flushSync();

    expect(document.querySelector('.dock'), 'the prompt has the box').not.toBeNull();

    harness.page.record = record({
      pending_asks: [permissionAsk()],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'push it once CI is green', truncated: false },
        compacting: false,
        sign_in: null,
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
        compacting: false,
        sign_in: null,
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
            composer: { take: null, notice: null, compacting: true, sign_in: null },
          }),
        },
        'Compacting context...',
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
  });

  it('rests as one row when it has nothing to put in a footer', () => {
    open();

    expect(
      document.querySelector('.foot'),
      'a box with no controls and no draft drew a footer row',
    ).toBeNull();
  });

  it('puts every control in the box footer, so the draft keeps the whole width', () => {
    open({ dictation: true, record: record({ header: { turn_in_flight: true } }) });
    type('a draft');

    for (const control of ['.line .mic', '.line .stop', '.line .send']) {
      expect(
        document.querySelector(control),
        `${control} still holds a column of the draft`,
      ).toBeNull();
    }
    for (const control of ['.foot .mic', '.foot .stop', '.foot .send']) {
      expect(
        document.querySelector(control),
        `${control} left the box rather than moving to its footer`,
      ).not.toBeNull();
    }
  });

  /**
   * The footer draws for a draft alone: an install that cannot dictate has no
   * mic, and a gate tightened to `dictation || running` would take the send and
   * both hints with it - the reachable case, since the router makes dictation
   * false whenever the wire omits it.
   */
  it('draws the send and the keys for a draft alone, with no dictation', () => {
    open();
    type('a draft');

    expect(document.querySelector('.foot .send'), 'a draft drew no way to send it').not.toBeNull();
    expect(document.querySelector('.foot'), 'the keys went with the mic').not.toBeNull();
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
      composer: { take, notice: null, compacting: false, sign_in: null, bind, mode },
    });

  /**
   * The bound key's own event, as the browser delivers it.
   *
   * jsdom reports no platform, so the box takes the non-mac substitution: the
   * right Control key is what Cmd's equivalent is where there is no Cmd.
   */
  function key(code: string, kind: 'keydown' | 'keyup', repeat = false): void {
    window.dispatchEvent(
      new KeyboardEvent(kind, { code, repeat, bubbles: true, cancelable: true }),
    );
    flushSync();
  }

  /**
   * Let a take's start settle.
   *
   * Opening the microphone is a promise - the browser's permission round
   * trip - and the command goes once it resolves, so a test that read the
   * wire on the same tick would see a take that had not started.
   */
  async function opened(): Promise<void> {
    for (let turn = 0; turn < 4; turn += 1) await Promise.resolve();
    flushSync();
  }

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
            },
          },
        },
      ]);

      vi.advanceTimersByTime(500);
      key('ControlRight', 'keyup');
      expect(harness.sent.at(-1)?.command, 'a hold released transcribes what was said').toEqual({
        dictate_stop: { key: { org: 'Busytools', project: 'forge', label: 'lead' }, submit: true },
      });
    } finally {
      vi.useRealTimers();
    }
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

    const newest = () => document.querySelector('.tc .bars i:last-child')?.getAttribute('style');
    mic.held.onFrame?.(encodeFrame([0.9]));
    flushSync();
    expect(newest(), "a loud frame's own reading").toBe('height: 94%;');

    // A quieter frame the socket does not take: the record's levels never see
    // it, and the card's graph reads it anyway.
    shared.takes = false;
    mic.held.onFrame?.(encodeFrame([0.4]));
    flushSync();
    expect(newest(), 'and a quieter one, read where it was produced').toBe('height: 83%;');
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
              compacting: false,
              sign_in: null,
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

      expect(mic.held.stops, 'the take must let go of the microphone at once').toBe(1);
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
   * A stray modifier is not a chord. The terminal consumes any other bare
   * modifier without marking the hold chorded, because a modifier is not text
   * and not a shortcut - and the client marking one would DISCARD the take its
   * press began, losing the reader's words to a key they brushed.
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
   * A live take consumes Escape, which is the terminal's rule and why the
   * client's surfaces cannot both have it: the list under the field closes on
   * an Escape nobody else took, and the dock's own Escape is the take's.
   */
  it('gives Escape to a live take rather than to the list underneath it', () => {
    const harness = open({ dictation: true });
    harness.page.record = bound('right_cmd', 'auto', take());
    flushSync();
    type('/');
    expect(document.querySelector('.ac'), 'a list is open under the field').not.toBeNull();

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
   * A held key repeats, and the terminal treats every repeat as carrying no
   * instruction. Without the same here a toggle take stops once per repeat:
   * one stopping press and two repeats gave three stops for one keypress.
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
        composer: { take: take(), notice: null, compacting: false, sign_in: null },
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
   * the record is handed over. `landed === held.text` is what holds it, and
   * without that the words would be appended again on every re-render.
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
  it('lands the words once while the notice is held', () => {
    const harness = open();
    seesTake(harness);
    harness.page.record = withNotice(LANDED);
    flushSync();
    expect(field().value, 'the take lands in the box').toBe(LANDED.text);

    harness.page.record = withNotice(LANDED);
    flushSync();

    expect(field().value, 'the words land once, not once per record handed over').toBe(LANDED.text);
  });

  it('still takes a landing whose words are new', () => {
    const harness = open();
    seesTake(harness);
    harness.page.record = withNotice(LANDED);
    flushSync();
    sendBox();

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
  it("does not put a sent take's words back when the reader leaves the seat and returns", () => {
    const harness = open();
    seesTake(harness);
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
 * the seat is. `forge-tui` holds the same state the same way, on `UiSession`
 * rather than on `App`, which is what makes a draft come back with its seat
 * and stops one seat's words reaching another's box.
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

  it('lands a take only on the seat that watched it, and lands it there', () => {
    const harness = open();
    // The seat's own take, which is what arms a landing for THIS box.
    seesTake(harness);

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
   * The move itself is not enough when the destination box has watched a take
   * of its own before, which is every seat the reader dictates on: the flag is
   * per box and never clears, so what is left stopping the landing is which
   * seat the record in hand belongs to - and right after a move that record is
   * still the seat being LEFT's, until the one being moved to answers.
   */
  it('does not land the left seat take in the box of the seat it moved to', () => {
    const harness = open();
    // Both seats are the reader's, so both boxes have watched a take: a fresh
    // box would refuse the landing below for the wrong reason.
    seesTake(harness);
    harness.page.slot = ELSEWHERE;
    harness.page.record = withNotice(null, take(), ELSEWHERE);
    flushSync();

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
  const rest = selector.replace(/^\.box/, '');
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

/** A selector naming the box itself: the class is bounded after its letters. */
const BOX = /^\.box(?![a-z0-9-])/;

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
      '.box:focus-within',
      '.box.warn { border-color: var(--warn); }\n.box:focus-within',
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
      '.box:focus-within',
      '.box[data-phase="warn"] { border-color: var(--warn); }\n.box:focus-within',
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
    const spliced = sheet.replace('.box.rec', '.box.reset { border: 0; }\n.box.rec');
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
   * row's TEXT sat 25.00px right of the field's, at 1600 and at 430 - the
   * terminal's gutter, carried through the drawing rather than chosen, and
   * 13px more than the issue that found it believed. A sheet assertion cannot
   * see that offset; what it can do is keep the inset from coming back, which
   * is the change someone would plausibly make.
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
  it('makes the box the container the card ladder measures', () => {
    expect(baseRule('.box'), 'the box is not a container, so no step can fire').toContain(
      'container-type: inline-size',
    );
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
      '.tc .bars, .tc .ticks, .tc .bar',
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
      '.box.rec',
      '.box.shadow { box-shadow: inset 0 -2px 0 var(--accent); }\n.box.rec',
    );
    expect(insetRules(spliced), 'a borderless rule for the box draws the inset line').toEqual([
      '.box.shadow',
    ]);
  });

  /**
   * The class name is bounded at both ends of a selector, so a neighbour's
   * class beginning with the same letters is not read as the box - reading it
   * would throw and take every test here with it.
   */
  it("does not read a neighbour's class as the box", () => {
    const spliced = sheet.replace('.box.rec', '.boxrow { border-color: var(--warn); }\n.box.rec');
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
    for (const grouped of ['.box.rec, .other {', '.other, .box.rec {']) {
      const spliced = sheet.replace('.box.rec {', grouped);
      expect(ring(spliced, ['rec'], false), `a rule grouped as ${JSON.stringify(grouped)}`).toBe(
        colour,
      );
    }
  });

  it('fails on a border shorthand it cannot read, rather than skipping it', () => {
    const spliced = sheet.replace(
      '.box.rec',
      '.box.odd { border: 1px solid var(--ok) inset; }\n.box.rec',
    );
    expect(
      () => ring(spliced, ['rec'], false),
      'a border shorthand this resolver cannot read is skipped',
    ).toThrow(/cannot read/);
  });

  it('puts the controls in the box footer, off the draft and under the caret', () => {
    expect(
      sheetRule('.foot .ctls'),
      'the controls lost the footer row the draft no longer shares',
    ).toContain('margin-left: auto');
    expect(() => sheetRule('.line .mic'), 'the mic still holds a column of the draft open').toThrow(
      /writes no rule/,
    );
  });

  /**
   * A narrow box cannot hold the hints and the controls on one row - measured,
   * the widest pair needs 365px - so the keys go and the controls keep the row.
   */
  it('drops the key hints rather than the controls when the box is narrow', () => {
    expect(sheet, 'the foot wraps its hints instead of giving them up').toMatch(
      /@container \(max-width: 400px\) \{ \.foot \.hints \{ display: none; \} \}/,
    );
  });

  /**
   * **The phone's floors are read on the block itself**, because the coarse
   * rules sit on one line inside the media query, where the per-selector scan
   * above reaches only their base rules - and that is exactly how they went
   * unpinned while two changes crossed.
   */
  it("keeps the mic and the panel's close at a finger's size, and the mic's accent", () => {
    const coarse = /@media \(pointer: coarse\) \{[^\n]*\.foot \.mic[^\n]*\}/.exec(sheet)?.[0] ?? '';
    expect(coarse, 'the mic lost its height floor on a finger').toContain('min-height: 44px');
    expect(coarse, 'the mic lost its width floor on a finger').toContain('min-width: 44px');
    expect(coarse, "the panel's close lost its width floor").toContain(
      '.pop .hd .x { min-width: 44px; }',
    );
    expect(sheetRule('.foot .mic:hover'), 'the mic lost its accent hover').toContain(
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

  it("gives the footer's controls the same pointer floor", () => {
    for (const control of ['.foot .mic', '.stop', '.send']) {
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
    // The terminal pops its prompt at submit; this dock stands instead, with
    // the mark the reader's own words carry while they are out - so a second
    // Enter is not read as a second answer to a prompt already answered.
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

  it('draws the custom answer as a row of its own, which opens the words rather than answering', () => {
    const harness = open({
      record: record({ pending_asks: [questionAsk('tu-q', { multi_select: false })] }),
    });

    expect(
      [...document.querySelectorAll('.dock .opt .lbl')].map((held) => held.textContent),
      'the custom answer sits last, where every other choice is',
    ).toEqual(['Staging', 'Production', 'Tell the agent something else']);

    options()[2]?.click();
    flushSync();
    expect(commands(harness), 'the door itself answers nothing').toEqual([]);

    const notes = document.querySelector('.dock .notes');
    if (!(notes instanceof HTMLTextAreaElement)) throw new Error('no words row');
    expect(document.activeElement, 'the door put the caret in the words').toBe(notes);

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

  it('opens the words on Space over the custom row, rather than toggling an id of its own', () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    // The custom row is the last one: an ArrowUp from the first wraps to it.
    press('ArrowUp');
    flushSync();
    press(' ');
    flushSync();

    expect(commands(harness), 'Space on the door answers nothing').toEqual([]);
    const notes = document.querySelector('.dock .notes');
    expect(document.activeElement, 'the door put the caret in the words').toBe(notes);
    expect(drawn(), 'and no count rides the key for a row that cannot be turned on').not.toContain(
      'submit 1',
    );
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
  it("puts a take's words in the box that is actually showing", () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    /** The dock's own box, which is the one the reader opened. */
    const dockBox = (): HTMLTextAreaElement | null => {
      const found = document.querySelector('.dock [data-editor="dock"]');
      return found instanceof HTMLTextAreaElement ? found : null;
    };

    expect(dockBox()?.value, 'the box the reader opened starts empty').toBe('');

    // The reader is in the row, which is what makes this dock the destination.
    const opened = dockBox();
    if (opened === null) throw new Error('the dock drew no words row');
    opened.focus();
    flushSync();

    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
    });
    flushSync();
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'the words', truncated: false },
        compacting: false,
        sign_in: null,
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
  it("lands a take's words in the dock when the take began under the question", () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
    });
    flushSync();
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'custom words', truncated: false },
        compacting: false,
        sign_in: null,
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
  it('gives each take its own destination: a later take under the question lands there', () => {
    const harness = open();

    // Take one begins with no prompt, so its words belong to the draft.
    harness.page.record = record({
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
    });
    flushSync();
    harness.page.record = record({
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'first words', truncated: false },
        compacting: false,
        sign_in: null,
      },
    });
    flushSync();

    // Then a question arrives, and take two begins under it.
    harness.page.record = record({ pending_asks: [questionAsk()] });
    flushSync();
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
    });
    flushSync();
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'second words', truncated: false },
        compacting: false,
        sign_in: null,
      },
    });
    flushSync();

    const dockBox = document.querySelector('.dock [data-editor="dock"]');
    expect(
      dockBox instanceof HTMLTextAreaElement ? dockBox.value : null,
      'the second take landed where IT began, not where the first one did',
    ).toBe('second words');
  });

  it('gives each take its own destination: a later take begun in the composer stays there', () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    // Take one begins under the question and lands in its row.
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
    });
    flushSync();
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'first words', truncated: false },
        compacting: false,
        sign_in: null,
      },
    });
    flushSync();

    // The question resolves, and take two begins in the composer.
    harness.page.record = record();
    flushSync();
    harness.page.record = record({
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
    });
    flushSync();

    // A question arrives mid-flight; this take's words are still the draft's.
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
    });
    flushSync();
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'second words', truncated: false },
        compacting: false,
        sign_in: null,
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
  it('captures the next take after one ended while another seat was shown', () => {
    const harness = open();

    // Take one begins with no prompt, so its words belong to the draft.
    harness.page.record = record({
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
    });
    flushSync();

    // The reader moves to another seat, and the take ends while they are away.
    harness.page.slot = ELSEWHERE;
    flushSync();
    harness.page.record = record({
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'first words', truncated: false },
        compacting: false,
        sign_in: null,
      },
    });
    flushSync();

    // They come back with a question up and take two ALREADY live under it, in
    // one frame - nothing this seat's box sees between the two takes.
    harness.page.slot = SLOT;
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
    });
    flushSync();
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'second words', truncated: false },
        compacting: false,
        sign_in: null,
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
  it('lands in the draft when the question resolved before the take did', () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
    });
    flushSync();
    harness.page.record = record({
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
    });
    flushSync();
    harness.page.record = record({
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'orphan words', truncated: false },
        compacting: false,
        sign_in: null,
      },
    });
    flushSync();

    expect(field().value, 'the words went to the box that still exists').toContain('orphan words');
  });

  it('lands in the draft under a permission that offers no words row', () => {
    const bare = permissionAsk('tu-1', {
      options: [
        { option_id: 'opt-once', name: 'Allow once', kind: 'allow', action: { kind: 'allow' } },
        { option_id: 'opt-deny', name: 'Deny', kind: 'deny', action: { kind: 'deny' } },
      ],
    });
    const harness = open({ record: record({ pending_asks: [bare] }) });

    harness.page.record = record({
      pending_asks: [bare],
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
    });
    flushSync();
    harness.page.record = record({
      pending_asks: [bare],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'draft words', truncated: false },
        compacting: false,
        sign_in: null,
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
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
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
        compacting: false,
        sign_in: null,
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
  it("keeps a take's words when the blocker takes the dock away before they land", () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

    expect(
      document.querySelector('.dock [data-editor="dock"]'),
      'the dock draws the box the words would go in',
    ).not.toBeNull();

    harness.page.seat = seatRead({ lifecycle: 'Failed', reason: 'the CLI exited with status 1' });
    flushSync();
    expect(document.querySelector('.dock'), 'and then the dock is gone').toBeNull();

    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
    });
    flushSync();
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'the words', truncated: false },
        compacting: false,
        sign_in: null,
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
  it("brings the keyboard back to the dock's box when a take lands in it", () => {
    const harness = open({ record: record({ pending_asks: [questionAsk()] }) });

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

    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
    });
    flushSync();
    harness.page.record = record({
      pending_asks: [questionAsk()],
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'the words', truncated: false },
        compacting: false,
        sign_in: null,
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
    void harness;
  });

  it('keeps a running take visible while a prompt holds the slot, and abandons it on escape', () => {
    const harness = open({
      record: record({
        pending_asks: [permissionAsk()],
        composer: { take: take(), notice: null, compacting: false, sign_in: null },
      }),
    });

    expect(
      document.querySelector('.dock .custom .tc'),
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
      why: 'that Slack draft is no longer waiting: it has been answered, or it expired',
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
          ending: 'expired',
        },
      },
    });
    flushSync();
    expect(drawn(), 'nothing is drawn on the seat showing now').not.toContain('expired unanswered');

    harness.page.slot = SLOT;
    harness.page.record = record();
    flushSync();

    expect(drawn(), 'and the seat it happened to meets the line on return').toContain(
      'expired unanswered',
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
    expect(drawn()).toContain('browser hand-off · job-hunt');
    expect(drawn(), "the reason is the session's own words, in full").toContain(
      'The sign-in page is showing a CAPTCHA.',
    );
    expect(drawn(), "Open is the client's own door").toContain('Open browser');
    expect(drawn(), 'and both answers are reachable').toContain('Done');
    expect(drawn()).toContain('Not now');
  });

  it('says why an answer did not land when the hand-off left under the click', () => {
    const harness = open({ record: record({ pending_asks: [browserHandOffAsk()] }) });

    action('Done').click();
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

  it('answers the hand-off as the verb says: Done settles it', () => {
    const harness = open({ record: record({ pending_asks: [browserHandOffAsk()] }) });

    action('Done').click();
    flushSync();

    expect(commands(harness)).toEqual([
      {
        respond_browser_hand_off: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          id: '0192e1c0-0000-7000-8000-0000000000aa',
          done: true,
        },
      },
    ]);
    // **Answering also lowers the window** (Ved live, 2026-10-07: "I said
    // done, but the browser tab is still open"): Open raised it, and the
    // answer is what takes it down. This line was the live bug's fix and
    // nothing pinned it.
    expect(hideBrowser, 'the window comes down with the answer').toHaveBeenCalledTimes(1);
    // **The hand-off's own profile, not the shared one**: lowering the wrong
    // window would leave the person's browser up while the answer said done.
    expect(hideBrowser, "and it is that hand-off's profile that comes down").toHaveBeenCalledWith(
      'job-hunt',
    );
  });

  /** **An answer from another view lowers the window here.** The local Done
   *  takes its own window down; a Done from the terminal never touches this
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
          ending: { type: 'done' },
        },
      },
    });

    expect(hideBrowser, 'the hand-off profile comes down').toHaveBeenCalledWith('job-hunt');
  });

  it('declines the hand-off with Not now, which is the same release', () => {
    const harness = open({ record: record({ pending_asks: [browserHandOffAsk()] }) });

    action('Not now').click();
    flushSync();

    expect(commands(harness)).toEqual([
      {
        respond_browser_hand_off: {
          key: { org: 'Busytools', project: 'forge', label: 'lead' },
          id: '0192e1c0-0000-7000-8000-0000000000aa',
          done: false,
        },
      },
    ]);
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
          done: false,
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
          ending: { type: 'done' },
        },
      },
    });
    harness.page.record = record();
    flushSync();

    expect(document.querySelector('.dock'), 'the dock stands down').toBeNull();
    expect(drawn(), 'and the row says which ending took it').toContain('settled in another view');
  });

  it("does not narrate the reader's own answer to a hand-off", () => {
    const harness = open({ record: record({ pending_asks: [browserHandOffAsk()] }) });

    action('Done').click();
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
          ending: { type: 'done' },
        },
      },
    });
    harness.page.record = record();
    flushSync();

    expect(drawn(), "the reader's own click is not another view's").not.toContain('another view');
  });

  /** **The window, not an overlay** (Ved, 2026-10-07): Open raises the
   * person's own browser over the profile the agents drive. */
  it('opens the browser window', async () => {
    vi.mocked(showBrowser).mockResolvedValueOnce(null);
    open({ record: record({ pending_asks: [browserHandOffAsk()] }) });

    action('Open browser').click();
    await vi.waitFor(() =>
      expect(showBrowser, 'the raise the approval is for').toHaveBeenCalledWith('job-hunt'),
    );
  });

  /** A window that will not open says why, in the shell's own words: the
   *  sentence a machine with no browser to drive carries names what to
   *  install, and a dock that replaced it with boilerplate would throw the
   *  one actionable line away. */
  it("says the raise failed in the shell's own words", async () => {
    const harness = open({ record: record({ pending_asks: [browserHandOffAsk()] }) });
    vi.mocked(showBrowser).mockResolvedValueOnce(
      'no browser to drive: this client drives the browser already on the machine - install Brave or Google Chrome',
    );

    action('Open browser').click();
    await vi.waitFor(() => expect(drawn()).toContain('install Brave or Google Chrome'));

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
    const harness = open({ record: record({ pending_asks: [questionAsk()] }), dictation: true });

    await micPress();
    expect(
      harness.sent.map((entry) => entry.command),
      'the press opens the microphone',
    ).toMatchObject([
      { dictate_stream: { key: { org: 'Busytools', project: 'forge', label: 'lead' } } },
    ]);

    await micPress();
    expect(harness.sent.at(-1)?.command, 'and the second press submits it').toMatchObject({
      dictate_stop: { key: { org: 'Busytools', project: 'forge', label: 'lead' }, submit: true },
    });
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
        composer: { take: take(), notice: null, compacting: false, sign_in: null },
      });
      flushSync();
      harness.page.record = record({
        pending_asks: [held],
        composer: {
          take: null,
          notice: { kind: 'landed', text: 'the dictated answer', truncated: false },
          compacting: false,
          sign_in: null,
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
      'the rows carry their own numbers, the first row one, the custom answer last',
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
        composer: { take: take(), notice: null, compacting: false, sign_in: null },
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
 * The list that opens over the box: four triggers, one popover shape.
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
    ).toEqual([
      '/mode Set session mode',
      '/model Show / set session model',
      '/usage Token/cost usage by project or model',
    ]);
    expect(document.querySelector('.ac .h .n')?.textContent, 'the header counts the matches').toBe(
      '3',
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
   * A seat's notice is the server's, not the client's, so the client that
   * never saw the take has no draft those words belong to. Landing them there
   * is the reader's own message arriving back in a box they did not type it in.
   */
  it('leaves the box empty on the client that never saw the take', () => {
    const { one, other } = openBoth();

    one.page.record = record({
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
    });
    flushSync();

    const landed = record({
      composer: {
        take: null,
        notice: { kind: 'landed', text: 'push it once CI is green', truncated: false },
        compacting: false,
        sign_in: null,
      },
    });
    one.page.record = landed;
    other.page.record = landed;
    flushSync();

    const [watched, missed] = bothFields();
    expect(watched?.value, 'the client that watched the take lands it').toBe(
      'push it once CI is green',
    );
    expect(missed?.value, 'the one that attached after it does not').toBe('');
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
 * The form is the mockup's rather than the terminal's: each axis is a short
 * exclusive set, so each is a row of chips. `dictation.test.ts` pins what the
 * axes and the hint ARE; this pins what the panel DOES with them.
 */
describe('the dictation panel', () => {
  /**
   * The axes and the input are the CLIENT's since capture moved here, so these
   * cases drive them through the storage a reload would read and through the
   * browser's own input list - never through the record, which carries the
   * terminal's set and nothing this page obeys.
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
        composer: { take: null, notice: null, compacting: false, sign_in: null, mode: 'hold' },
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
        composer: { take: null, notice: null, compacting: false, sign_in: null, bind: 'left_cmd' },
      }),
    });
    expect(document.querySelector('.pop .hd')?.textContent).toContain('left to talk');

    void unmount(app as Record<string, unknown>);
    app = null;
    document.body.innerHTML = '';
    opened({
      record: record({
        composer: { take: null, notice: null, compacting: false, sign_in: null, bind: 'off' },
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
   * The cap follows the COMPOSER, not just the window.
   *
   * A list opening above the field, or a take row landing above it, grows the
   * composer and lifts the panel with it - and a cap measured on mount would
   * stay where it was and cut the panel's own top off. jsdom has no
   * `ResizeObserver` and performs no layout, so what this pins is that one is
   * watching the composer; that the cap then tracks a growing box is a browser
   * measurement, and the body carries its numbers.
   */
  it('watches the composer, so a box that grows under the panel re-measures it', () => {
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
        'nothing watches the composer, so a box growing under the panel re-clips it',
      ).toBeGreaterThan(0);
      expect(observed[0]?.classList.contains('comp'), 'the composer is what is watched').toBe(true);
    } finally {
      Reflect.set(globalThis, 'ResizeObserver', original);
    }
  });

  /**
   * And the cap READS what the composer reports.
   *
   * The assertion above pins that something watches; this pins that the watch
   * does something - the regression the whole cap exists for. jsdom performs no
   * layout, but it does not have to: the rect is supplied as an own property on
   * the panel, so the component's own call answers what a browser would have
   * measured, and the callback a fake observer was handed is fired by hand.
   */
  it('caps the panel from the bottom the composer reports', () => {
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
      if (!(pop instanceof HTMLElement)) throw new Error('the panel drew nothing to cap');
      Object.defineProperty(pop, 'getBoundingClientRect', {
        configurable: true,
        value: () => ({
          bottom: 500,
          top: 0,
          left: 0,
          right: 0,
          width: 0,
          height: 0,
          x: 0,
          y: 0,
          toJSON: () => ({}),
        }),
      });

      const fire = callbacks.at(0);
      if (fire === undefined) throw new Error('nothing watched the composer');
      fire();
      flushSync();

      expect(pop.style.maxHeight, 'the cap reads the measured bottom').toBe('494px');
    } finally {
      Reflect.set(globalThis, 'ResizeObserver', original);
    }
  });
});
