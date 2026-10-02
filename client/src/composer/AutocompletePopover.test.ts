// @vitest-environment jsdom
/**
 * The popover as it is drawn: what a heading above a source does to the rows
 * underneath it, and what a mark move - or a list rebuilt under one - does to
 * the window they scroll in.
 *
 * **Named around `Autocomplete.svelte`'s own module test rather than after the
 * component.** `Autocomplete.test.ts` and `autocomplete.test.ts` are one file
 * on a case-insensitive filesystem, which is what this machine has - writing
 * the second name silently overwrites the first.
 */
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import Autocomplete from './Autocomplete.svelte';
import AutocompleteHarness from './AutocompleteHarness.svelte';
import { offer, rowId, type Offer, type Sources } from './autocomplete';
import type { FileEntry } from './wire';

/** A `/` list with both sources, which is the list that draws a heading. */
const sources = (): Sources => ({
  forgeCommands: [
    { name: '/compact', description: 'Compact session context' },
    { name: '/mode', description: 'Set session mode' },
    { name: '/model', description: 'Show / set session model' },
  ],
  advertised: [
    { name: '/clear', description: 'Clear chat history' },
    { name: '/help', description: 'Show help' },
  ],
  files: [],
  agents: [],
});

/** The list every test here reads out of the DOM rather than out of the offer. */
function held(): Offer {
  const found = offer('/', sources);
  if (found === null) throw new Error('the bare trigger offers nothing');
  return found;
}

/** One file, as the `@` list reads it. */
function file(relPath: string): FileEntry {
  const basename = relPath.split('/').pop() ?? relPath;
  return {
    relPath,
    relPathLower: relPath.toLowerCase(),
    basenameLower: basename.toLowerCase(),
    depth: relPath.split('/').length - 1,
  };
}

/** The `@` list's sources, which is the list whose rows change under a fixed query. */
const files =
  (paths: string[]): (() => Sources) =>
  () => ({
    forgeCommands: [],
    advertised: [],
    files: paths.map(file),
    agents: [],
  });

/** The list a draft opens, or a throw, since every test here is about one. */
function opened(draft: string, from: () => Sources): Offer {
  const found = offer(draft, from);
  if (found === null) throw new Error(`${draft} offers nothing`);
  return found;
}

/** The window the rows scroll in, which is the element a mark moves inside. */
function window_(): HTMLElement {
  const found = document.querySelector('.rows');
  if (!(found instanceof HTMLElement)) throw new Error('the popover is not drawing its rows');
  return found;
}

/**
 * Give an element the box jsdom cannot: jsdom performs no layout, so every
 * rectangle is zero and every window is nothing tall.
 *
 * The numbers the scroll tests feed it are the ones measured on the real page
 * at 800x620, so the arithmetic is checked against a box a browser drew rather
 * than one this file invented.
 */
function place(element: Element, top: number, bottom: number, options: { window?: number } = {}) {
  Object.defineProperty(element, 'getBoundingClientRect', {
    configurable: true,
    value: () => ({ top, bottom, height: bottom - top, left: 0, right: 0, width: 0, x: 0, y: top }),
  });
  if (options.window !== undefined) {
    Object.defineProperty(element, 'clientHeight', { configurable: true, value: options.window });
    Object.defineProperty(element, 'scrollTop', { configurable: true, writable: true, value: 0 });
  }
}

/** The harness's own mark and list, which a test sets the way the composer does. */
function pageOf(instance: Record<string, unknown>): { at: number; offer: Offer } {
  return (instance as { page: { at: number; offer: Offer } }).page;
}

let drawn: Record<string, unknown> | null = null;

afterEach(() => {
  if (drawn !== null) void unmount(drawn);
  drawn = null;
  document.body.innerHTML = '';
});

describe('what the popover draws', () => {
  it('heads each source without heading a row of its own', () => {
    const list = held();
    drawn = mount(Autocomplete, {
      target: document.body,
      props: { offer: list, marked: 2, onpick: () => {} },
    });

    expect(
      [...window_().querySelectorAll('.grp')].map((label) => label.textContent),
      'each source carries its heading once',
    ).toEqual(['forge', 'cli']);
    expect(
      window_().querySelectorAll('[role="option"]').length,
      'a heading is not a candidate, so the mark still counts rows alone',
    ).toBe(list.rows.length);
    expect(
      window_().getAttribute('aria-activedescendant'),
      'and the mark names the row it counted to',
    ).toBe(rowId(list, 2));
    expect(document.querySelector('.it.sel')?.id, 'which is the row drawn as marked').toBe(
      rowId(list, 2),
    );
  });
});

/**
 * The list follows the mark, which the mouse never needed: a wheel scrolls the
 * window under it, and a mark nothing scrolled to walks off the edge it
 * crossed - by a key, and by a query that rebuilds the rows under a mark whose
 * own index never moved. The terminal's picker keeps the mark on screen by
 * windowing its list around it and re-clamping on every update, and this is
 * that rule on a scrolling element.
 */
describe('the window a mark moves in', () => {
  it('follows the mark to the edge it crossed, and no further', () => {
    const list = held();
    const last = list.rows.length - 1;
    drawn = mount(AutocompleteHarness, {
      target: document.body,
      props: { offer: list, onpick: () => {} },
    });

    const box = window_();
    // What the page measured: a 248px window over a 499px list, with the last
    // row's box 465.5px down it - 217px past the bottom edge.
    place(box, 0, 248, { window: 248 });
    const marked = document.getElementById(rowId(list, last));
    if (marked === null) throw new Error('the popover is not drawing its last row');
    place(marked, 465.5, 498.8);

    pageOf(drawn).at = last;
    flushSync();

    expect(
      box.scrollTop,
      'the offset that lands the marked row on the bottom edge it crossed',
    ).toBeCloseTo(498.8 - 248, 1);
  });

  it('brings the mark back when a query rebuilds the list under it', () => {
    const list = held();
    drawn = mount(AutocompleteHarness, {
      target: document.body,
      props: { offer: list, onpick: () => {} },
    });
    // Settle the mount first: the effect is queued until a flush, so a test that
    // skips this passes on that first run whatever the list does afterwards.
    flushSync();

    const box = window_();
    place(box, 0, 248, { window: 248 });
    const row = document.getElementById(rowId(list, 0));
    if (row === null) throw new Error('the popover is not drawing its first row');
    // The measured state after typing into a list wheeled to its bottom: the
    // list shrank, the browser clamped the offset, and the mark - which never
    // moved - was left well above the window.
    place(row, -251, -217.7);
    box.scrollTop = 251;
    expect(box.scrollTop, 'the wheel left the list where the browser clamped it').toBe(251);

    const narrowed = offer('/c', sources);
    if (narrowed === null) throw new Error('the narrowed query offers nothing');
    pageOf(drawn).offer = narrowed;
    flushSync();

    expect(box.scrollTop, 'a rebuilt list puts its mark back on the top edge').toBe(0);
  });

  it('brings the mark back when a rebuild changes how many rows matched', () => {
    // The `@` list is the live case: its rows are rebuilt from an index that
    // refreshes under a draft nobody has touched, so the count moves while the
    // kind and the query do not.
    const thin = opened('@sz', files(['a/sz.rs', 'b/sz.rs', 'c/sz.rs']));
    drawn = mount(AutocompleteHarness, {
      target: document.body,
      props: { offer: thin, onpick: () => {} },
    });
    flushSync();

    const box = window_();
    place(box, 0, 248, { window: 248 });
    const row = document.getElementById(rowId(thin, 0));
    if (row === null) throw new Error('the popover is not drawing its first row');
    place(row, -251, -217.7);
    box.scrollTop = 251;

    const grown = opened('@sz', files(['a/sz.rs', 'new/sz.rs', 'b/sz.rs', 'c/sz.rs']));
    pageOf(drawn).offer = grown;
    flushSync();

    expect(box.scrollTop, 'a list that grew under the mark brings it back').toBe(0);
  });

  it('leaves the reader where they are when a frame changes nothing about the list', () => {
    const list = held();
    drawn = mount(AutocompleteHarness, {
      target: document.body,
      props: { offer: list, onpick: () => {} },
    });
    flushSync();

    const box = window_();
    place(box, 0, 248, { window: 248 });
    const row = document.getElementById(rowId(list, 0));
    if (row === null) throw new Error('the popover is not drawing its first row');
    place(row, -251, -217.7);
    box.scrollTop = 251;

    // A frame arrives as a fresh offer object carrying the same kind, query and
    // count - the composer can hand one over with nothing about the list moved -
    // and none of the three changed, so the reader's scroll stands.
    pageOf(drawn).offer = held();
    flushSync();

    expect(
      box.scrollTop,
      'a frame that moves neither kind, query nor count leaves the scroll alone',
    ).toBe(251);
  });

  it('leaves the offset alone while the mark is inside, so a wheel scroll stands', () => {
    const list = held();
    drawn = mount(AutocompleteHarness, {
      target: document.body,
      props: { offer: list, onpick: () => {} },
    });

    const box = window_();
    place(box, 0, 248, { window: 248 });
    const row = document.getElementById(rowId(list, 1));
    if (row === null) throw new Error('the popover is not drawing the row');
    place(row, 100, 133.3);
    // The reader has wheeled the list 150px down, and the mark moves to a row
    // that scroll left in the window.
    box.scrollTop = 150;

    pageOf(drawn).at = 1;
    flushSync();

    expect(box.scrollTop, 'a row already in the window is not a reason to move it').toBe(150);
  });
});
