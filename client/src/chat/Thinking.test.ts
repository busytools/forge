import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import { ICONS } from '../components/sprite';
import Thinking from './Thinking.svelte';

/**
 * The thinking row, as markup.
 *
 * **What this can answer and what it cannot.** The row's kind is carried by a
 * WORD, so the thing to assert is text: a disclosure takes its accessible name
 * from its summary, and a reader who cannot see the row has only those words to
 * tell reasoning from a tool call. The mark's own size and place are layout,
 * which jsdom does not perform - the by-width measurement is where that is
 * answered.
 */

const WORDS = 'The socket holds one conversation per seat, and a refresh re-reads it';

const SHEET = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');

const body = render(Thinking, { props: { text: `${WORDS} rather than replaying it.` } }).body;

/** The summary's own words, with Svelte's block markers and the tags off. */
const named = ((): string => {
  const open = body.indexOf('<summary');
  const close = body.indexOf('</summary>');
  return body
    .slice(open, close)
    .replace(/<!--.*?-->/g, '')
    .replace(/<[^>]*>/g, '');
})();

describe('the thinking row', () => {
  it('names its kind before it shows its own words', () => {
    // It was the only row in the column whose kind had to be inferred from its
    // sentence - and the one thing a screen reader is given is that sentence.
    expect(named, 'the row says what it is').toMatch(/^thinking\b/);
    expect(named, 'and still carries the words it is about').toContain(WORDS.slice(0, 40));
  });

  it('draws that mark from the page sprite rather than a glyph of its own', () => {
    expect(body, 'the mark is the sprite reference').toContain('href="#i-think"');
    expect(ICONS, 'and the sprite carries it').toContain('think');
  });

  it('wears a class the sheet does not already own', () => {
    // **A bare class name reaches further than the row it was written for.**
    // Drawn as `.kind`, this span inherited the family-tree disclosure's 3px
    // margins and made every thinking row 6px taller - the trap the sheet's
    // own `.knd` was split out of. So the class the row wears must have no
    // bare rule of that name anywhere the page reads: not at the top level,
    // not inside a media query, and not as one member of a selector list.
    expect(body, 'the kind is drawn with its own class').toContain('class="tkind"');
    for (const [what, source] of sheets()) {
      expect(bareRules(source, 'tkind'), `${what} has no bare .tkind rule`).toEqual([]);
    }
    expect(bareRules(SHEET, 'kind'), 'where the name it must not take is taken').not.toEqual([]);
    expect(
      bareRules(SHEET, 'pbody'),
      'and a control: a name the scan must find wherever it is written',
    ).not.toEqual([]);
  });
});

/** The two sheets a page reads: the app's, and the book's own drawing of it. */
function sheets(): Array<[string, string]> {
  return [
    ['web.css', SHEET],
    [
      'the book drawing',
      readFileSync(
        new URL('../../../docs/book/src/ui/client/web-session.html', import.meta.url),
        'utf8',
      ),
    ],
  ];
}

/**
 * Every selector member whose FIRST compound is that class, wherever it is
 * written.
 *
 * **A rule is not always at the top level and not always alone in its
 * selector.** `@media (min-width: 1px) { .tkind { ... } }` reaches the row
 * exactly as a top-level rule does, and `.tkind, .other { ... }` is a member of
 * a list that a name-only scan walks past - both were holes a name-grep left
 * open, and both are the defect this test exists to catch.
 */
function bareRules(sheet: string, name: string): string[] {
  const code = sheet.replace(/\/\*[\s\S]*?\*\//g, '');
  const found: string[] = [];
  for (const rule of code.matchAll(/([^{}]+)\{/g)) {
    for (const member of (rule[1] ?? '').split(',')) {
      const first = member.trim().split(/[\s>+~]+/)[0] ?? '';
      if (first === `.${name}`) found.push(member.trim());
    }
  }
  return found;
}
