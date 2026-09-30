// @vitest-environment jsdom
import { readFileSync } from 'node:fs';
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
 * How the box says it has the keyboard. The composer focuses its field on
 * mount, so the resting look IS the focused look, which is why the accent
 * moving off the frame is the change rather than a detail of it.
 */
describe('the frame', () => {
  it('keeps the accent off the frame at rest and draws it along the bottom edge on focus', () => {
    expect(
      sheetRule('.box'),
      'the resting frame is not the control border every other control rests at',
    ).toContain('border: 1.5px solid var(--ctl)');

    const focus = sheetRule('.box:focus-within');
    expect(focus, 'nothing on the box says where the keyboard is').toContain(
      'inset 0 -2px 0 var(--accent)',
    );
    expect(
      focus,
      'focus still paints the whole 1.5px frame, so the box shouts again',
    ).not.toContain('border-color');
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
