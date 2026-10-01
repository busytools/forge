// @vitest-environment jsdom
import { readFileSync } from 'node:fs';
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import Harness from './Harness.svelte';
import type { ServerMessage } from '../protocol';
import { permissionAsk, questionAsk, record, seatRead, take, wire, type Wire } from './testing';
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
  document.body.innerHTML = '';
});

/** The harness's own state, which a test sets the way a page would re-render it. */
interface Page {
  record: ComposerRecord;
  seat: SeatRead;
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

/** Both clients' boxes, in mount order, which is one field per composer. */
function bothFields(): HTMLTextAreaElement[] {
  return [...document.querySelectorAll('textarea')].map((found) => {
    if (!(found instanceof HTMLTextAreaElement)) throw new Error('a box drew no field');
    return found;
  });
}

/** Every option row the page is drawing, in order. */
function options(): HTMLElement[] {
  return [...document.querySelectorAll('.opt')].map((row) => {
    if (!(row instanceof HTMLElement)) throw new Error('an option row is not an element');
    return row;
  });
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

    harness.page.record = record({ pending_ask: permissionAsk() });
    flushSync();

    expect(
      document.querySelector('textarea'),
      'the dock morphs the box, so it draws no field of its own',
    ).toBeNull();
    expect(drawn(), 'the prompt itself is what the slot draws').toContain('Allow once');

    const answered = document.querySelector('.opt .lbl');
    if (!(answered instanceof HTMLElement))
      throw new Error('the dock drew no option to answer with');
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

    harness.page.record = record({ pending_ask: permissionAsk() });
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

    expect(harness.sent, 'the command is the text as typed, addressed to this seat').toEqual([
      {
        command: {
          prompt: {
            key: { org: 'Busytools', project: 'forge', label: 'lead' },
            text: 'push it once CI is green',
            attachments: [],
          },
        },
      },
    ]);
    expect(field().value, 'the box is empty once the words have gone').toBe('');
  });

  it("grows by the take's own row and collapses when the take resolves", () => {
    const harness = open();
    const before = document.querySelector('.box')?.innerHTML ?? '';

    harness.page.record = record({
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
    });
    flushSync();

    expect(document.querySelector('.box .dict'), 'the row lives inside the box').not.toBeNull();
    expect(document.querySelector('.box')?.innerHTML, 'the box grew a row').not.toBe(before);

    harness.page.record = record();
    flushSync();

    expect(document.querySelector('.dict'), 'the row collapses with the take').toBeNull();
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

    const watching = record({
      pending_ask: permissionAsk(),
      composer: { take: take(), notice: null, compacting: false, sign_in: null },
    });
    harness.page.record = watching;
    flushSync();

    expect(document.querySelector('.dock'), 'the prompt has the box').not.toBeNull();

    harness.page.record = record({
      pending_ask: permissionAsk(),
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
      [{ seat: seatRead({ lifecycle: 'Spawning' }) }, 'Connecting to Claude Code...', null],
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
        // The shape the session page actually hands over for a seat nothing has
        // started: no roster row, so no lifecycle and `waking` true together.
        {
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

  it('rests as one row, because a footer with nothing to say is a strip of its own', () => {
    open({ dictation: true });

    expect(
      document.querySelector('.foot'),
      'an empty box draws a footer row whose only content is the microphone',
    ).toBeNull();
  });

  it('puts the way into a take on the input line rather than in a row of its own', () => {
    open({ dictation: true });

    expect(
      document.querySelector('.line .mic'),
      'the microphone sits on its own strip below the draft',
    ).not.toBeNull();
  });

  it('offers the way in only when this install can dictate, and the way in is the door', () => {
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
    mic.click();
    flushSync();

    expect(document.querySelector('.pop'), 'the mic is the door, not the trigger').not.toBeNull();
    expect(harness.sent, 'and nothing on the page starts a take').toEqual([]);
  });
});

/**
 * The push-to-talk key, which is the only thing that starts a take.
 *
 * The binding and the mode are read off the record rather than assumed: they
 * are the user's `forge.toml`, and a page that hardcoded a chord would honour a
 * different key on every install that moved it. `dictate-key.test.ts` pins what
 * a press MEANS; this pins what the box DOES with it.
 */
describe('the key', () => {
  /** A record whose composer carries the binding and the mode under test. */
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

  it('starts a take on the bound key, and transcribes it when the key is held', () => {
    vi.useFakeTimers();
    try {
      const harness = open({ dictation: true });
      harness.page.record = bound('right_cmd', 'auto');
      flushSync();

      key('ControlRight', 'keydown');
      expect(harness.sent, 'the key is how a take begins').toEqual([
        {
          command: {
            dictate_start: { key: { org: 'Busytools', project: 'forge', label: 'lead' } },
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

  it('takes the binding off the record rather than assuming one', () => {
    const harness = open({ dictation: true });
    harness.page.record = bound('left_cmd', 'auto');
    flushSync();

    key('ControlRight', 'keydown');
    expect(harness.sent, 'the key the config did not name is not the trigger').toEqual([]);

    key('ControlLeft', 'keydown');
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

  it('leaves a chord alone: the take its press began is abandoned, not transcribed', () => {
    const harness = open({ dictation: true });
    harness.page.record = bound('right_cmd', 'auto');
    flushSync();

    key('ControlRight', 'keydown');
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
  it('leaves a stray modifier alone rather than chording the take', () => {
    vi.useFakeTimers();
    try {
      const harness = open({ dictation: true });
      harness.page.record = bound('right_cmd', 'auto');
      flushSync();

      key('ControlRight', 'keydown');
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
        pending_ask: permissionAsk(),
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
  /** A record whose composer holds what the case wants it to. */
  const withNotice = (notice: unknown, held: Record<string, unknown> | null = null) =>
    record({
      composer: { take: held, notice, compacting: false, sign_in: null },
    });

  const LANDED = { kind: 'landed', text: 'push it once CI is green', truncated: false };

  /** Send what is in the box, as the reader's Enter does. */
  function sendBox(): void {
    field().dispatchEvent(
      new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }),
    );
    flushSync();
  }

  /** A take the box has seen, which is what arms the landing. */
  function seesTake(harness: { page: { record: ComposerRecord } }): void {
    harness.page.record = withNotice(null, take());
    flushSync();
  }

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

  it('puts the controls on the last line, so they follow the caret down', () => {
    expect(
      sheetRule('.line'),
      'the controls sit in the middle of a grown draft rather than on the line the caret is on',
    ).toContain('align-items: flex-end');
  });

  it('keeps the separation the rows above the draft had before C moved the field and the footer', () => {
    expect(sheet, 'a row above the draft lost the 8px the box used to give it').toMatch(
      /\.dict \+ \.line[^{]*\{[^}]*margin-top: 8px/,
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

  it('gives both controls on the input line the same pointer floor', () => {
    for (const control of ['.line .mic', '.send']) {
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

  it('answers a question with the row that was clicked, and only that row', () => {
    const harness = open({
      record: record({ pending_ask: questionAsk('tu-q', { multi_select: false }) }),
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
    const harness = open({ record: record({ pending_ask: questionAsk() }) });

    options()[0]?.click();
    options()[1]?.click();
    flushSync();

    expect(
      [...document.querySelectorAll('.opt .box2')].map((box) => box.classList.contains('on')),
      'the boxes carry what is toggled',
    ).toEqual([true, true, false]);
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
    const harness = open({ record: record({ pending_ask: questionAsk() }) });
    const list = document.querySelector('.dock [role="listbox"]');
    if (!(list instanceof HTMLElement)) throw new Error('the dock drew no listbox');

    // The listbox is what holds the keys by default; a row an earlier click
    // focused is the other path, and it is reachable - `tabindex="-1"` takes
    // focus on click, just not by Tab. This leg is the listbox's.
    press(' ');
    expect(
      [...document.querySelectorAll('.dock .box2')].map((box) => box.classList.contains('on')),
      'space turns the marked row on',
    ).toEqual([true, false, false]);

    press(' ');
    expect(
      [...document.querySelectorAll('.dock .box2')].map((box) => box.classList.contains('on')),
      'and off again',
    ).toEqual([false, false, false]);
    expect(harness.sent, 'a toggle is never an answer').toEqual([]);
  });

  it('toggles once when the row itself holds the focus, rather than twice', () => {
    open({ record: record({ pending_ask: questionAsk() }) });

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
    ).toEqual([true, false, false]);
  });

  it('keeps the keyboard with the mark, so a key after an arrow acts on the marked row', () => {
    open({ record: record({ pending_ask: questionAsk() }) });

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
    ).toEqual([false, true, false]);
  });

  it('answers from Enter on a focused row, rather than only toggling it', () => {
    const harness = open({ record: record({ pending_ask: questionAsk() }) });

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
    ).toEqual([false, false, false]);
  });

  it('opens the own-words field rather than rejecting when Enter lands there with nothing said', () => {
    const harness = open({ record: record({ pending_ask: questionAsk() }) });

    const own = options()[2];
    if (!(own instanceof HTMLElement)) throw new Error('the dock drew no own-words row');
    own.focus();
    flushSync();

    press('Enter');

    expect(harness.sent, 'nothing said is not an answer, and not a rejection').toEqual([]);
    expect(document.activeElement, 'so the row hands over the field to write in').toBe(
      document.querySelector('.dock textarea.notes'),
    );
  });

  it('moves the mark back out of the own-words field, so the options are reachable again', () => {
    const harness = open({ record: record({ pending_ask: questionAsk() }) });
    const list = document.querySelector('.dock [role="listbox"]');
    if (!(list instanceof HTMLElement)) throw new Error('the dock drew no listbox');

    // One ArrowUp from the load state wraps onto the own-words row, which hands
    // the keyboard to the field. From there the arrows have to keep working, or
    // the field is a trap with no way back to the options.
    press('ArrowUp');
    const field = document.querySelector('.dock textarea.notes');
    expect(document.activeElement, 'the own-words row hands over the keyboard').toBe(field);

    press('ArrowUp');

    expect(document.activeElement, 'and a key hands it back').toBe(list);
    expect(document.querySelector('.dock .opt.sel')?.textContent, 'one row further up').toContain(
      'Production',
    );
    expect(harness.sent, 'moving the mark answers nothing').toEqual([]);
  });

  it("does not deny on the reader's behalf from a permission's own-words row", () => {
    const harness = open({ record: record({ pending_ask: permissionAsk() }) });

    options()[2]?.click();
    flushSync();
    press('Enter');

    expect(harness.sent, 'nothing said is nothing sent').toEqual([]);
  });

  it("carries the reader's own words as the answer's annotation, not as an option", () => {
    const harness = open({ record: record({ pending_ask: questionAsk() }) });

    const own = options()[2];
    own?.click();
    flushSync();

    const notes = document.querySelector('.dock .notes');
    if (!(notes instanceof HTMLTextAreaElement)) {
      throw new Error('the own-words row drew no field to write in');
    }
    expect(commands(harness), 'a row that asks for words does not answer without them').toEqual([]);

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

  it("hands a permission's own-words row the text it promised", () => {
    const harness = open({ record: record({ pending_ask: permissionAsk() }) });

    const own = options()[2];
    if (!(own instanceof HTMLElement)) throw new Error('the dock drew fewer rows than it offers');
    own.click();
    flushSync();

    const notes = document.querySelector('.dock .notes');
    if (!(notes instanceof HTMLTextAreaElement)) {
      throw new Error('the own-words row drew no field to write in');
    }
    notes.value = 'not this branch';
    notes.dispatchEvent(new Event('input', { bubbles: true }));
    press('Enter');

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

    harness.page.record = record({ pending_ask: permissionAsk() });
    flushSync();

    const list = document.querySelector('.dock [role="listbox"]');
    expect(document.activeElement, 'the dock owns the keyboard it was handed').toBe(list);
    expect(document.querySelector('textarea'), 'and the box is not beside it').toBeNull();

    harness.page.record = record();
    flushSync();

    expect(document.activeElement, 'the box takes the keyboard back').toBe(field());
  });

  it('moves the mark from the keyboard once the dock has the slot', () => {
    const harness = open({ record: record({ pending_ask: permissionAsk() }) });
    const list = document.querySelector('.dock [role="listbox"]');
    if (!(list instanceof HTMLElement)) throw new Error('the dock drew no listbox');

    list.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true }));
    flushSync();

    expect(document.querySelector('.opt.sel')?.textContent).toContain('Deny');
    expect(
      list.getAttribute('aria-activedescendant'),
      'and the listbox points at the row it moved to',
    ).toContain('opt-deny');
    void harness;
  });

  it('keeps a running take visible while a prompt holds the slot, and abandons it on escape', () => {
    const harness = open({
      record: record({
        pending_ask: permissionAsk(),
        composer: { take: take(), notice: null, compacting: false, sign_in: null },
      }),
    });

    expect(drawn(), 'the take is named rather than swallowed').toContain('dictating');
    expect(
      document.querySelector('.dock .blip .dot'),
      'and it says it is still listening',
    ).not.toBeNull();

    const list = document.querySelector('.dock [role="listbox"]');
    if (!(list instanceof HTMLElement)) throw new Error('the dock drew no listbox');
    list.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    flushSync();

    expect(commands(harness), 'the first escape belongs to the take').toEqual([
      {
        dictate_stop: { key: { org: 'Busytools', project: 'forge', label: 'lead' }, submit: false },
      },
    ]);
  });

  it('says why, and gives the words back, when the core refuses a send', () => {
    const harness = open();
    type('push it once CI is green');
    press('Enter');
    expect(field().value, 'the box is cleared for the next thing').toBe('');

    harness.say({ kind: 'error', what: 'dispatch', why: 'the session is not running' });
    flushSync();

    expect(field().value, 'the words come back with the refusal').toBe('push it once CI is green');
    expect(drawn()).toContain('the session is not running');
  });

  it('names a held post for what is waiting rather than explaining an empty dock', () => {
    open({
      record: record({
        pending_ask: {
          kind: 'slack_draft',
          request: {
            workspace: 'Trust Machines',
            conversation_label: 'granite-staging-alerts',
            thread_ts: null,
            text: 'Deploy finished on staging.',
            tool: 'slack__post',
          },
        },
      }),
    });

    expect(drawn()).toContain('Post to Slack');
    expect(drawn()).toContain('Trust Machines · granite-staging-alerts');
    expect(drawn()).toContain('slack__post');
    expect(drawn(), 'and does not claim options this view never drew').not.toContain(
      'arrived before this view attached',
    );
  });

  it("draws the question's own mark for its header, not a character-cell glyph", () => {
    open({ record: record({ pending_ask: questionAsk() }) });

    expect(document.querySelector('.dock .qm use')?.getAttribute('href')).toBe('#i-question');
    expect(drawn(), 'and the queue line carries no glyph').not.toContain('▼');
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
      '/mode Show / set session mode',
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
    const asked = record({ pending_ask: permissionAsk() });
    one.page.record = asked;
    other.page.record = asked;
    flushSync();

    expect(options(), 'both clients draw the prompt').toHaveLength(6);

    const allow = options()[0];
    if (allow === undefined) throw new Error('the dock drew no options');
    allow.click();
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
    const asked = record({ pending_ask: permissionAsk() });
    one.page.record = asked;
    other.page.record = asked;
    flushSync();

    options()[1]?.click();
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
    one.page.record = record({ pending_ask: permissionAsk() });
    flushSync();

    options()[0]?.click();
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
  /** The panel, opened by the mic, which is the only way in. */
  function opened(over: Partial<ComposerProps> = {}, on?: Wire) {
    const harness = open({ dictation: true, ...over }, on);
    const mic = document.querySelector('.mic');
    if (!(mic instanceof HTMLElement)) throw new Error('the box drew no mic to open with');
    mic.click();
    flushSync();
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

  it('draws the in-force value on every axis, from the record rather than a default', () => {
    opened({
      record: record({
        dictate_overrides: { styling: 'casual', structure: 'lists', context: null },
      }),
    });

    const on = chips().filter((chip) => chip.on);
    expect(on.map((chip) => `${chip.label}`)).toEqual([
      'casual',
      'may bullet a list',
      'plain text',
    ]);
  });

  it('marks an axis the session set, and only that one', () => {
    opened({
      record: record({
        dictate_overrides: { styling: 'formal', structure: null, context: null },
      }),
    });

    // The mode and the device carry a source tag of their own - they come from
    // the config rather than from this session - so the tag is read by its
    // words rather than by the class alone.
    const marked = [...document.querySelectorAll('.pop .lbl .src')]
      .filter((held) => held.textContent?.includes('this session') === true)
      .map((held) => words(held.parentElement));
    expect(marked, 'the source tag names the axes this session moved').toEqual([
      'VOICE · this session',
    ]);
  });

  it('asks the core for one axis when a chip is clicked', () => {
    const harness = opened();

    const chip = [...document.querySelectorAll('.pop .chip')].find(
      (held) => held.textContent?.trim() === 'casual',
    );
    if (!(chip instanceof HTMLElement)) throw new Error('the voice axis drew no casual chip');
    chip.click();
    flushSync();

    expect(harness.sent, 'one chip, one axis, in the core own vocabulary').toEqual([
      {
        command: {
          set_dictate_override: {
            key: { org: 'Busytools', project: 'forge', label: 'lead' },
            update: { styling: 'casual' },
          },
        },
      },
    ]);
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

  it('asks for the devices once, and picks one by its id', () => {
    const shared = wire();
    const harness = opened({ device: { device: 'mic-2' } }, shared);
    expect(document.querySelector('.pop .dev')?.textContent, 'the pick the home carries').toContain(
      'mic-2',
    );

    const door = document.querySelector('.pop .dev');
    if (!(door instanceof HTMLElement)) throw new Error('the panel drew no device row');
    door.click();
    flushSync();
    expect(shared.asked, 'one ask for one walk').toBe(1);

    harness.say({
      kind: 'devices',
      devices: [
        { id: 'mic-2', name: 'Shure SM7B', is_default: false },
        { id: 'mic-9', name: 'MacBook Pro Microphone', is_default: true },
      ],
      configured: 'mic-2',
    });
    flushSync();

    expect(
      document.querySelector('.pop .dev')?.textContent,
      'the row names the device the walk found',
    ).toContain('Shure SM7B');

    const row = [...document.querySelectorAll('.pop .row')].find((held) =>
      held.textContent?.includes('MacBook Pro Microphone'),
    );
    if (!(row instanceof HTMLElement)) throw new Error('the list drew no second device');
    row.click();
    flushSync();

    expect(harness.sent.at(-1)?.command, 'a pick names the id, which is the identity').toEqual({
      set_dictate_device: {
        key: { org: 'Busytools', project: 'forge', label: 'lead' },
        pick: { device: 'mic-9' },
      },
    });
  });

  it('resets every axis at once', () => {
    const harness = opened();
    const reset = document.querySelector('.pop .rst');
    if (!(reset instanceof HTMLElement)) throw new Error('the panel drew no reset');
    reset.click();
    flushSync();

    expect(harness.sent).toEqual([
      {
        command: {
          reset_dictate_overrides: {
            key: { org: 'Busytools', project: 'forge', label: 'lead' },
          },
        },
      },
    ]);
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

  /**
   * A walk that failed. The socket's own contract is explicit - the refusal is
   * rendered where the list would have been - so an empty list region and a
   * failed walk must not draw the same. This is the composer's own pattern for
   * a refused dispatch, one file over.
   */
  it('draws a failed walk in the list region rather than as an empty list', () => {
    const shared = wire();
    const harness = opened({}, shared);

    const door = document.querySelector('.pop .dev');
    if (!(door instanceof HTMLElement)) throw new Error('the panel drew no device row');
    door.click();
    flushSync();

    harness.say({ kind: 'error', what: 'devices', why: 'no permission to the microphone' });
    flushSync();

    const region = document.querySelector('.pop .list');
    expect(region?.textContent, 'the refusal is drawn where the list would be').toContain(
      'no permission to the microphone',
    );
    expect(
      region?.textContent,
      'and a failed walk does not read as a walk that found nothing',
    ).not.toContain('No input devices found');
  });

  it('asks once however many times the row is clicked, and closes on the next', () => {
    const shared = wire();
    opened({}, shared);
    const door = document.querySelector('.pop .dev');
    if (!(door instanceof HTMLElement)) throw new Error('the panel drew no device row');

    // Three clicks before any answer: each ask opens the microphone stack, so
    // only the first may leave the panel.
    door.click();
    flushSync();
    door.click();
    door.click();
    flushSync();
    expect(shared.asked, 'the walk is the expensive part, so it is asked for once').toBe(1);

    shared.say({
      kind: 'devices',
      devices: [{ id: 'mic-9', name: 'MacBook Pro Microphone', is_default: true }],
      configured: null,
    });
    flushSync();

    const again = document.querySelector('.pop .dev');
    if (!(again instanceof HTMLElement)) throw new Error('the row went with the list');
    again.click();
    flushSync();
    expect(document.querySelector('.pop .list'), 'a click collapses it again').toBeNull();
    expect(shared.asked, 'and collapsing asks for nothing').toBe(1);
  });

  it('marks an absent input, and words a pin differently from a pick', () => {
    const devices = [{ id: 'mic-9', name: 'MacBook Pro Microphone', is_default: true }];

    const pinned = wire();
    opened({}, pinned);
    const door = document.querySelector('.pop .dev');
    if (!(door instanceof HTMLElement)) throw new Error('the panel drew no device row');
    door.click();
    flushSync();
    pinned.say({ kind: 'devices', devices, configured: 'unplugged-1' });
    flushSync();

    const row = document.querySelector('.pop .dev');
    expect(row?.textContent, 'the absent pin says where it came from').toContain(
      'not present · pinned in forge.toml',
    );
    expect(row?.classList.contains('missing'), 'and it is marked, not only worded').toBe(true);

    // A pick that is gone is the reader's own, and the terminal words it
    // without the pin's words: the two absences are not the same absence.
    void unmount(app as Record<string, unknown>);
    app = null;
    document.body.innerHTML = '';
    const picked = wire();
    opened({ device: { device: 'walked-off-2' } }, picked);
    const second = document.querySelector('.pop .dev');
    if (!(second instanceof HTMLElement)) throw new Error('the panel drew no device row');
    second.click();
    flushSync();
    picked.say({ kind: 'devices', devices, configured: 'mic-9' });
    flushSync();

    const pickedRow = document.querySelector('.pop .dev');
    expect(pickedRow?.textContent, 'the absent pick is named without the pin words').toContain(
      'not present',
    );
    expect(pickedRow?.textContent, 'and does not claim the config set it').not.toContain(
      'pinned in forge.toml',
    );
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
});
