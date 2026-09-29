// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import Harness from './Harness.svelte';
import type { ServerMessage } from '../protocol';
import { permissionAsk, questionAsk, record, seatRead, take, wire, type Wire } from './testing';
import type { ComposerProps, ComposerRecord, SeatRead } from './view';

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

/** Press a key on the surface the composer listens on, which is the field. */
function press(key: string): void {
  field().dispatchEvent(new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true }));
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

    harness.page.record = record({ pending_ask: questionAsk() });
    flushSync();

    const rejected = document.querySelectorAll('.opt .lbl')[1];
    if (!(rejected instanceof HTMLElement)) throw new Error('the dock drew one option only');
    rejected.click();
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

  it('grows by the take’s own row and collapses when the take resolves', () => {
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

  it('lands a take’s words at the caret and takes one green beat before easing back', () => {
    vi.useFakeTimers();
    try {
      const harness = open();
      type('fix the');

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
        'and the border eases back to the box’s own',
      ).toBe(false);
      expect(field().value, 'the words stay').toBe('fix the flaky retry test');
    } finally {
      vi.useRealTimers();
    }
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

  it('replaces the box entirely for each reason it cannot take keys, and says why', () => {
    const cases: [Partial<ComposerProps>, string, string | null][] = [
      [{ seat: seatRead({ lifecycle: 'Spawning' }) }, 'Connecting to Claude Code…', null],
      [
        {
          record: record({
            composer: { take: null, notice: null, compacting: true, sign_in: null },
          }),
        },
        'Compacting context…',
        null,
      ],
      [
        { seat: seatRead({ lifecycle: 'Failed', reason: 'the CLI exited with status 1' }) },
        'Input disabled due to error',
        'the CLI exited with status 1',
      ],
      [
        { seat: seatRead({ waking: true, reason: 'no session has been started here' }) },
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

  it('offers the way into a take only when this install can dictate', () => {
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

    expect(harness.sent, 'the way in starts the take it offers').toEqual([
      {
        command: { dictate_start: { key: { org: 'Busytools', project: 'forge', label: 'lead' } } },
      },
    ]);
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
      'forge’s table is what it offers, matched on the description too',
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

    expect(shared.sent, 'one answer went, and it is the core’s own option').toEqual([
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
