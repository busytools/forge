// @vitest-environment jsdom
/**
 * The popover as it is drawn: what a heading above a source does to the rows
 * underneath it.
 *
 * **Named around `Autocomplete.svelte`'s own module test rather than after the
 * component.** `Autocomplete.test.ts` and `autocomplete.test.ts` are one file
 * on a case-insensitive filesystem, which is what this machine has - writing
 * the second name silently overwrites the first.
 */
import { mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import Autocomplete from './Autocomplete.svelte';
import { offer, rowId, type Offer, type Sources } from './autocomplete';

/** A `/` list with both sources, which is the list that draws a heading. */
const sources = (): Sources => ({
  forgeCommands: [
    { name: '/compact', description: 'Compact session context' },
    { name: '/mode', description: 'Show / set session mode' },
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

/** The window the rows are drawn in, which is the element a mark counts in. */
function window_(): HTMLElement {
  const found = document.querySelector('.rows');
  if (!(found instanceof HTMLElement)) throw new Error('the popover is not drawing its rows');
  return found;
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
