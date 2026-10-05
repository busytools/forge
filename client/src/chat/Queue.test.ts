// @vitest-environment jsdom
import { readFileSync } from 'node:fs';

import axe from 'axe-core';
import { JSDOM } from 'jsdom';
import { flushSync, mount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import type { Connection } from '../socket';
import type { QueueEnding, QueuedPromptRow } from '../session/wire';
import type { SessionSlot } from '../wire/types';
import Queue from './Queue.svelte';
import { faceAt, walk } from './queue';
import Composed from './testing/Composed.svelte';

/**
 * The pile: the rows the core holds, the walk's arithmetic, and what this
 * view does with them.
 *
 * **What leaves the pile is the core's answer, not this view's**: a cancel
 * dispatches and the row stays until the stream settles it, which is why the
 * cancel tests assert the DISPATCH rather than an empty pile.
 */

const SLOT: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

const row = (uuid: string): QueuedPromptRow => ({ uuid, source: 'you', text: uuid });

const rows = (...texts: string[]): QueuedPromptRow[] =>
  texts.map((text, at) => ({ uuid: `u${at}`, source: 'you', text }));

/** A connection that records what the component dispatched. */
function recording(): { sent: unknown[]; connection: Connection } {
  const sent: unknown[] = [];
  const connection = {
    dispatch: (command: unknown) => {
      sent.push({ command });
    },
  } as unknown as Connection;
  return { sent, connection };
}

let host: HTMLElement;

beforeEach(() => {
  // Inside a landmark: axe's `region` rule is about the page, and a bare body
  // with one component on it is a finding about this harness rather than
  // about the pile.
  const main = document.createElement('main');
  host = document.createElement('div');
  main.append(host);
  document.body.append(main);
});

afterEach(() => {
  document.body.innerHTML = '';
});

function draw(
  held: QueuedPromptRow[],
  connection: Connection,
  ended: QueueEnding | null = null,
): void {
  const app = mount(Queue, {
    target: host,
    props: { rows: held, ended, slot: SLOT, connection },
  });
  flushSync();
  // The unmount rides the afterEach that empties the host.
  void app;
}

function pile(): HTMLElement {
  const found = host.querySelector<HTMLElement>('.pile [role="listbox"]');
  if (found === null) throw new Error('the pile did not draw');
  return found;
}

/** The composed harness's own state, which a test moves the way a page re-renders. */
function pageOf(instance: Record<string, unknown>): { rows: QueuedPromptRow[] } {
  const held = instance['page'];
  if (held === null || typeof held !== 'object') throw new Error('the harness exposed no props');
  return held as { rows: QueuedPromptRow[] };
}

function key(target: HTMLElement, name: string): void {
  target.dispatchEvent(
    new KeyboardEvent('keydown', { key: name, bubbles: true, cancelable: true }),
  );
  flushSync();
}

describe('the walk', () => {
  const three = [row('oldest'), row('middle'), row('newest')];

  it('enters at the newest from the box, and stops on the first queued prompt', () => {
    expect(walk(null, three, 'up')).toBe('newest');
    expect(walk('newest', three, 'up')).toBe('middle');
    expect(walk('middle', three, 'up')).toBe('oldest');
    expect(walk('oldest', three, 'up'), 'the first queued prompt is where it stops').toBe('oldest');
  });

  it('comes back down and lands in the box past the newest', () => {
    expect(walk('oldest', three, 'down')).toBe('middle');
    expect(walk('newest', three, 'down'), 'past the newest is the box').toBeNull();
  });

  it('re-arms at the newest when the pointed row left', () => {
    // The cursor names a row the core settled under the walk: it is no longer
    // in the pile, and a walk that indexed into it would move nothing at all.
    expect(walk('delivered', three, 'up')).toBeNull();
    expect(walk('delivered', three, 'down')).toBeNull();
  });
});

describe('the face', () => {
  const three = [row('oldest'), row('middle'), row('newest')];

  it('is the cursor row when the walk is on it', () => {
    expect(faceAt('middle', three)).toBe(1);
  });

  it('is the newest when the walk is in the box, or on a row that has left', () => {
    expect(faceAt(null, three)).toBe(2);
    expect(faceAt('delivered', three)).toBe(2);
  });
});

describe('the queue pile', () => {
  it('draws nothing when nothing is waiting', () => {
    const { connection } = recording();
    draw([], connection);

    expect(host.querySelector('.pile')).toBeNull();
  });

  it('walks from the newest to the oldest and back into the box', () => {
    const { connection } = recording();
    draw(rows('one', 'two'), connection);

    key(pile(), 'ArrowUp');
    expect(
      pile().querySelector('.qcard.cur')?.getAttribute('id'),
      'the first up walks to the newest row',
    ).toBe('u1');

    key(pile(), 'ArrowUp');
    expect(pile().querySelector('.qcard.cur')?.getAttribute('id'), 'the second walks older').toBe(
      'u0',
    );

    key(pile(), 'ArrowUp');
    expect(
      pile().querySelector('.qcard.cur')?.getAttribute('id'),
      'up stops on the first queued prompt',
    ).toBe('u0');

    key(pile(), 'ArrowDown');
    expect(
      pile().querySelector('.qcard.cur')?.getAttribute('id'),
      'down returns to the newest',
    ).toBe('u1');
    key(pile(), 'ArrowDown');
    expect(pile().querySelector('.qcard.cur'), 'down past the newest lands in the box').toBeNull();
  });

  it('walks the face: its words, its place in the pile, and the cancel it offers', () => {
    const { sent, connection } = recording();
    draw(rows('one', 'two', 'three'), connection);

    key(pile(), 'ArrowUp');
    const face = pile().querySelector('.qcard.cur');
    expect(face?.querySelector('.w')?.textContent, 'the face is the walked row').toBe('three');
    expect(face?.querySelector('.pos')?.textContent, 'and says where that row sits').toBe('#3 / 3');

    key(pile(), 'ArrowUp');
    // The runner spells its rows' sources, so the second row's own label is
    // what the control should name.
    const control = host.querySelector<HTMLButtonElement>('.qcount .del');
    expect(control?.textContent?.trim(), 'the head offers to cancel the walked source').toBe(
      'cancel you',
    );
    control?.click();
    expect(sent, 'and dispatches the walked row, not the newest').toEqual([
      { command: { cancel_queued_prompt: { key: SLOT, uuid: 'u1' } } },
    ]);
  });

  it('dispatches a cancel for the row it is on, and leaves the row to the stream', () => {
    const { sent, connection } = recording();
    draw(rows('one'), connection);

    key(pile(), 'ArrowUp');
    key(pile(), 'Backspace');

    expect(sent).toEqual([{ command: { cancel_queued_prompt: { key: SLOT, uuid: 'u0' } } }]);
    expect(
      pile().querySelectorAll('.qcard'),
      'the row leaves when the core says so, not when the key does',
    ).toHaveLength(1);
  });

  it('makes the depth by the step, so a deep queue costs steps and not rows', () => {
    const { connection } = recording();
    draw(rows('one', 'two', 'three', 'four'), connection);

    const stack = host.querySelector<HTMLElement>('.qstack');
    expect(stack?.style.height, 'the face plus one step per older prompt').toBe(`${64 + 3 * 6}px`);

    const drawn = host.querySelectorAll<HTMLElement>('.qcard');
    expect(drawn[3]?.style.transform, 'the newest sits at the foot').toBe('translateY(0px)');
    expect(drawn[0]?.style.transform, 'the oldest is three steps above it').toBe(
      'translateY(-18px)',
    );
    expect(
      drawn[0]?.classList.contains('back'),
      'an older row is an edge: it keeps its words to itself',
    ).toBe(true);
  });

  it('says what an ending was, where the card was', () => {
    const { connection } = recording();
    mount(Queue, {
      target: host,
      props: {
        rows: [],
        ended: { text: 'the lost one', state: 'discarded' },
        slot: SLOT,
        connection,
      },
    });
    flushSync();

    expect(
      host.querySelector('.ended')?.textContent,
      'a row that vanishes silently reads as one that was delivered',
    ).toBe('discarded, never sent');
  });

  /**
   * The pile is the PR's one new interactive surface, and axe has to see it
   * WALKED: the walked markup is where the cursor row, its position and the
   * cancel control all exist, and the server render draws none of them.
   */
  it('walks with no axe violation over the walked markup', async () => {
    const { connection } = recording();
    draw(rows('one', 'two'), connection);
    key(pile(), 'ArrowUp');
    pile().focus();
    flushSync();

    const found = await axe.run(document.body);

    expect(found.violations.map((violation) => violation.id)).toEqual([]);
  });
});

/**
 * The key ring, inside the container the page composes - the shape the walk's
 * handbacks are structural about, and the one no mounting test held.
 */
describe('the walk and the box in one composer', () => {
  const composed = (initial: QueuedPromptRow[], connection: Connection) => {
    const app = mount(Composed, {
      target: host,
      props: { rows: initial, slot: SLOT, connection },
    });
    flushSync();
    const page = pageOf(app);
    return {
      page,
      box: () => host.querySelector('textarea'),
      pile: () => pile(),
    };
  };

  it('hands the keyboard back to the box on the last down and on the pile closing', () => {
    const { connection } = recording();
    const harness = composed(rows('one', 'two'), connection);

    harness.pile().focus();
    flushSync();
    expect(document.activeElement, 'the up-entry lands on the pile').toBe(harness.pile());

    // Down past the newest lands in the box - focus included, so the next
    // Enter is the next send.
    key(harness.pile(), 'ArrowDown');
    flushSync();
    expect(document.activeElement, 'the walk out returns the keyboard').toBe(harness.box());

    // And a pile that empties under a focused reader hands it back the same
    // way, rather than dropping it on the body with the next keystroke.
    harness.pile().focus();
    flushSync();
    harness.page.rows = [];
    flushSync();
    expect(document.activeElement, 'the pile going takes nothing with it').toBe(harness.box());
  });
});

/**
 * The pile's vocabulary is its own.
 *
 * **A namespace, not a preference.** The sheet carries bare `.row` (the home
 * list's own grid: `display: grid; grid-template-columns: var(--cols)`) and
 * bare `.hint` (the composer's), and a card that shared a name let a rule it
 * never declared - `display` - shrink its word line to a single character:
 * measured in the real engine against Ved's screenshot, `.w` drew 13px inside
 * a 498px card (#1705). The mock keeps the same namespaced set (`.qcard`,
 * `.qcount`, `.qstack`, `web-queue.html`), and this is the guard that the
 * sheet never grows a rule over one of them again.
 *
 * **The one shared name is `.ic`, on purpose**: `Icon.svelte` applies it to
 * every icon the app draws, the cancel control's included. Every other name
 * the sheet could reach the pile by, the pile must not carry.
 */
describe("the pile's own vocabulary", () => {
  /** The sheet's icon name, which `Icon.svelte` gives every icon there is. */
  const SHARED = new Set(['ic']);

  it('keeps every named rule off the pile, in the walked state too', () => {
    // The walked state is the one carrying the cancel control and its icon,
    // and the ended line belongs to the same check: guarding only the states
    // the app draws for a frame is how a live reach passed this test.
    draw(rows('one', 'two'), recording().connection, { text: 'gone', state: 'discarded' });
    key(pile(), 'ArrowUp');

    // Inside the composer, where the page mounts the pile: a rule anchored on
    // an ancestor the snapshot omits would pass a flatter tree.
    // From the suite's own root, which the runner sets to the client: jsdom
    // rewrites `import.meta.url` to a non-file URL, so the sheet has no file
    // URL to be read by, and vitest hands a css `?raw` import back empty.
    const sheet = readFileSync('src/assets/web.css', 'utf8');
    const dom = new JSDOM(`<style>${sheet}</style><div class="composer">${host.innerHTML}</div>`);
    const styles = dom.window.document.styleSheets[0];
    if (styles === undefined) throw new Error('the sheet did not parse');

    const reached: string[] = [];
    const check = (selectorText: string): void => {
      for (const part of selectorText.split(',')) {
        const selector = part.trim();
        // An element-only selector is the sheet's touch and type rules, not a
        // name the pile carries by accident - and it is where the reset and
        // its `*::before` companions sit, so they leave by the same door.
        if (!selector.includes('.') && !selector.includes('#') && !selector.includes('[')) {
          continue;
        }
        const names = selector.match(/\.[A-Za-z0-9_-]+/g) ?? [];
        if (names.length > 0 && names.every((name) => SHARED.has(name.slice(1)))) continue;
        for (const element of dom.window.document.querySelectorAll('.pile, .pile *')) {
          try {
            if (element.matches(selector)) {
              reached.push(
                `${selector} reaches ${element.getAttribute('class') ?? element.tagName}`,
              );
            }
          } catch {
            // A selector this reader cannot place is not evidence of a reach.
          }
        }
      }
    };
    // Counted at the walk, not read from the sheet: the sheet's own nested-rule
    // count stays greater than zero even when the descent below is removed, so
    // a denominator read off the sheet cannot die.
    let nested = 0;
    const visit = (rules: CSSRuleList, depth: number): void => {
      for (const rule of rules) {
        if (depth > 0) nested += 1;
        if ('selectorText' in rule) check((rule as CSSStyleRule).selectorText);
        const inner = 'cssRules' in rule ? (rule as CSSGroupingRule).cssRules : undefined;
        if (inner !== undefined && inner.length > 0) visit(inner, depth + 1);
      }
    };
    visit(styles.cssRules, 0);

    // Coverage assertions for the ways this check went blind: the snapshot
    // must hold the walked icon the check exists for, the pile must sit in
    // the composer an anchored rule could otherwise hide behind, the ended
    // line belongs to the drawn states, and the walk must have entered the
    // at-rule bodies a bare `.row` already hides in.
    expect(
      dom.window.document.querySelector('.qcount .del .ic'),
      'the walked cancel icon is the case this check reads',
    ).not.toBeNull();
    expect(
      dom.window.document.querySelector('.composer .pile'),
      'the check must read the pile inside its composer',
    ).not.toBeNull();
    expect(
      dom.window.document.querySelector('.pile .ended'),
      'the ended line is one of the states this check reads',
    ).not.toBeNull();
    expect(nested, "the walk read the sheet's at-rule bodies").toBeGreaterThan(0);
    expect(reached, 'the sheet must not reach into the pile').toEqual([]);
  });
});
