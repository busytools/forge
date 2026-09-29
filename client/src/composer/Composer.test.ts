// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import Composer from './Composer.svelte';
import { fake, permissionAsk, questionAsk, record, wire, type Wire } from './testing.svelte';
import type { ComposerProps } from './view';

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
  if (app !== null) unmount(app);
  if (second !== null) unmount(second);
  app = null;
  second = null;
  document.body.innerHTML = '';
});

function open(over: Partial<ComposerProps> = {}, on?: Wire) {
  const harness = fake(over, on);
  app = mount(Composer, { target: document.body, props: harness.props });
  flushSync();
  return harness;
}

/** The phone and the desktop: two composers, one seat, one connection. */
function openBoth(over: Partial<ComposerProps> = {}) {
  const shared = wire();
  const one = open(over, shared);
  const props = fake(over, shared);
  second = mount(Composer, { target: document.body, props: props.props });
  flushSync();
  return { shared, one, other: props };
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

    harness.props.record = record({ pending_ask: permissionAsk() });
    flushSync();

    expect(
      document.querySelector('textarea'),
      'the dock morphs the box, so it draws no field of its own',
    ).toBeNull();
    expect(drawn(), 'the prompt itself is what the slot draws').toContain('Allow once');

    const answered = document.querySelector('.opt .lbl');
    if (!(answered instanceof HTMLElement)) throw new Error('the dock drew no option to answer with');
    answered.click();
    flushSync();

    harness.props.record = record();
    flushSync();

    expect(
      field().value,
      'the reader typed this and the dock took it',
    ).toBe('fix the flaky retry test');
  });

  it('is held across a prompt the reader rejected', () => {
    const harness = open();
    type('ship it once CI is green');

    harness.props.record = record({ pending_ask: questionAsk() });
    flushSync();

    const rejected = document.querySelectorAll('.opt .lbl')[1];
    if (!(rejected instanceof HTMLElement)) throw new Error('the dock drew one option only');
    rejected.click();
    flushSync();

    harness.props.record = record();
    flushSync();

    expect(field().value, 'the draft went with the prompt').toBe('ship it once CI is green');
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
    one.props.record = asked;
    other.props.record = asked;
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
    one.props.record = asked;
    other.props.record = asked;
    flushSync();

    options()[1]?.click();
    flushSync();
    expect(shared.sent, 'the second client answered on its own').toHaveLength(1);

    // The core resolves once, and both pages re-read one prompt, so what the
    // other client draws is the read rather than anything this one told it.
    const gone = record();
    one.props.record = gone;
    other.props.record = gone;
    flushSync();

    expect(document.querySelectorAll('.opt'), 'the dock outlived the prompt').toHaveLength(0);
    expect(field(), 'and the box is back on both').toBeInstanceOf(HTMLTextAreaElement);
  });

  it('says why when the core refuses the answer', () => {
    const { shared, one } = openBoth();
    one.props.record = record({ pending_ask: permissionAsk() });
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
