// @vitest-environment jsdom
import { readFileSync } from 'node:fs';

import { JSDOM } from 'jsdom';
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import type { ReviewSetRecord, ReviewThreadRecord } from '../session/wire';
import ThreadTurn from './ThreadTurn.svelte';

/**
 * One turn of a thread, the piece both halves of the review page draw.
 *
 * These pin the two defects issue #2154 caught: the meta chip welded onto the
 * author's name (the canvas drew the name and the chip as adjacent spans with
 * no rule between them, so a turn read `youR1`), and the words drawn raw,
 * backticks and all, where the chat renders markdown.
 *
 * **What jsdom can decide about the chip is its box and its row's layout, not
 * geometry** - it lays nothing out - so the chip check is the sheet's own
 * cascade, read from the shipped file rather than an import, which comes back
 * empty under the runner.
 */

type Turn = ReviewThreadRecord['comments'][number];

const NOW = Date.parse('2026-10-13T00:30:00Z');

function turn(over: Partial<Turn> = {}): Turn {
  return {
    author: { kind: 'user' },
    text: 'the words',
    at: '2026-10-13T00:00:00Z',
    review_id: null,
    ...over,
  };
}

function round(id: string, number: number): ReviewSetRecord {
  return { id, number, summary: null, created_at: '2026-10-13T00:00:00Z' };
}

let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

/** The turn, mounted where the page mounts it: inside the review page's root. */
function draw(held: Turn, rounds: ReviewSetRecord[] = [], index = 0): HTMLElement {
  const page = document.createElement('div');
  page.className = 'review-page';
  document.body.append(page);
  app = mount(ThreadTurn, {
    target: page,
    props: { turn: held, rounds, index, now: NOW, thread: 't1' },
  });
  flushSync();
  return page;
}

/** What the sheet computes for one element of the render. */
function computed(host: HTMLElement, selector: string): CSSStyleDeclaration {
  const sheet = readFileSync('src/assets/web.css', 'utf8');
  const dom = new JSDOM(`<style>${sheet}</style><div class="review-page">${host.innerHTML}</div>`);
  const el = dom.window.document.querySelector(selector);
  if (el === null) throw new Error(`the render carries no ${selector}`);
  return dom.window.getComputedStyle(el);
}

describe('a turn of a thread', () => {
  it('keeps the name and its chip as two elements, each carrying its own words', () => {
    // The regression is the weld itself: a tag folded into the name's string,
    // or a chip drawn with no element of its own.
    const page = draw(turn({ review_id: 'r1' }), [round('r1', 1)]);
    expect(page.querySelector('.rv-nm')?.textContent, 'the name and nothing else').toBe('you');
    expect(page.querySelector('.rv-at')?.textContent, 'the round on its own').toBe('R1');
    expect(
      page.querySelector('.rv-who')?.children.length,
      'one element per token, so neither can be read as part of the other',
    ).toBe(2);
  });

  it('draws the chip as a box the sheet styles, not as a bare word', () => {
    const page = draw(turn({ review_id: 'r1' }), [round('r1', 1)]);
    expect(computed(page, '.rv-nm').borderRadius, 'the name is text, not a box').toBe('');
    expect(computed(page, '.rv-at').borderRadius, 'the chip carries a box of its own').not.toBe('');
    expect(computed(page, '.rv-at').paddingLeft, 'with its own inset').not.toBe('');
    const row = computed(page, '.rv-who');
    expect(row.display, 'and the row spaces what it holds').toBe('flex');
    expect(row.columnGap, 'by a gap, so the next chip is spaced too').not.toBe('');
  });

  it('stamps a turn with the round that sealed it, else with its age', () => {
    // The regression is a stamp that stops tracking the turn: an age on the
    // thread's opening turn (nothing to be later than) or on a sealed one.
    const sealed = draw(turn({ review_id: 'r1' }), [round('r1', 1)], 3);
    expect(sealed.querySelector('.rv-at')?.textContent).toBe('R1');
    const answered = draw(turn({ author: { kind: 'agent', label: 'impl' } }), [], 1);
    expect(answered.querySelector('.rv-at')?.textContent, 'thirty minutes on').toBe('30m');
    const opening = draw(turn({ author: { kind: 'agent', label: 'impl' } }), [], 0);
    expect(opening.querySelector('.rv-at'), 'the opening turn carries no stamp').toBeNull();
  });

  it('renders the words as the chat does, marks and lines alike', () => {
    // The regression is the other half of the sighting: `turn.text` reaching
    // the page as characters. The break is the chat's own-words rule - lines
    // someone typed into a comment are lines they meant.
    const page = draw(turn({ text: 'a `code` span and **emphasis**\nkept on its own line' }));
    const words = page.querySelector('.rv-turn .prose');
    expect(words?.querySelector('code')?.textContent).toBe('code');
    expect(words?.querySelector('strong')?.textContent).toBe('emphasis');
    expect(words?.querySelector('br'), "the author's newline is kept").not.toBeNull();
    expect(words?.textContent ?? '', 'and no mark reaches the page as a character').not.toContain(
      '`',
    );
  });
});
