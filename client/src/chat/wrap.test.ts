import { readFileSync } from 'node:fs';

import { JSDOM } from 'jsdom';
import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import Call from './Call.svelte';
import Code from './Code.svelte';
import Prose from './Prose.svelte';
import type { ToolLeaf } from './leaves';

/**
 * A long line in the conversation, as the sheet draws it.
 *
 * **What this can hold and what it cannot.** A scrollbar is not in the DOM and
 * a jsdom test performs no layout, so nothing here can see the overflow this
 * file is about: the scroll was measured in a browser instead, with the diff
 * box at 822px wide against 1740px of content before the change. What a test
 * can hold is the rule that decides it - and the change worth catching is
 * someone reading "code should not wrap" into a `white-space: pre` and bringing
 * the sideways scroll back.
 *
 * **The rules are matched against the components' own markup, wherever they are
 * written.** A name-anchored scan misses a rule inside a `@media` block and a
 * rule whose selector reaches the same element by another path - the shape this
 * file exists to catch, since the block the fix deleted lived in exactly that
 * shape. So every rule in the sheet is tried against the element with
 * `matches`, comments are stripped first, and a selector jsdom cannot read is
 * a failure rather than a silent skip.
 *
 * The terminal is the reference here (rule 24): it wraps a long code line
 * inside its panel, wraps a diff line and re-emits the indent on the
 * continuation row, and hard-splits a token with nowhere to break
 * (`wrap_long_token` in `crates/forge-tui/src/ui/wrap.rs`).
 */
const sheet = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');

/** A call carrying a diff and a command's own output, as the fold hands it over. */
const CALL = render(Call, {
  props: {
    call: {
      id: 'toolu_01',
      row: { kind: 'family', family: 'edit' },
      name: 'Edit',
      title: 'Edit src/lib.rs',
      command: null,
      status: 'completed',
      note: null,
      body: [
        { kind: 'diff', path: 'src/lib.rs', old: 'let a = 1;', new: 'let a = 2;' },
        { kind: 'text', text: 'ok' },
      ],
    } as ToolLeaf,
  },
}).body;

/** A source file a call read, as the panel draws it. */
const PANEL = render(Code, { props: { path: 'src/lib.rs', text: 'let a = 1;' } }).body;

/** A table in a message, as the markdown renderer draws one. */
const TABLE = render(Prose, { props: { text: '| a | b |\n| - | - |\n| c | d |' } }).body;

/**
 * Every declaration the sheet applies to the first `selector` in `html`,
 * joined.
 *
 * **The sheet is parsed, not grepped.** The rules come out of the browser's own
 * CSSOM, so a rule inside a `@media` block is reached on its own pass, a
 * selector list or a higher-specificity selector is matched with `matches`
 * rather than by name, and a commented out declaration is not a declaration.
 */
function declarationsFor(html: string, selector: string): string {
  const dom = new JSDOM(`<style>${sheet}</style>${html}`);
  const element = dom.window.document.querySelector(selector);
  expect(element, `${selector} is in the markup`).not.toBeNull();
  const styles = dom.window.document.styleSheets[0];
  expect(styles, 'the sheet parsed').not.toBeUndefined();
  if (element === null || styles === undefined) throw new Error('unreachable');
  return appliedTo(styles.cssRules, element);
}

/**
 * Every value `declarations` gives `property`, one per rule that sets it.
 *
 * **Presence is not the question.** A rule that decides the other way collects
 * beside the one this file names - a second copy inside a media query, or a
 * selector with more specificity than it - and the element then reads whatever
 * the cascade says, not whatever the first rule found says.
 */
function valuesOf(declarations: string, property: string): string[] {
  const set = new RegExp(`(?:^|[;{\\s])${property}:\\s*([^;}]+)`, 'g');
  return [...declarations.matchAll(set)].map((match) => (match[1] ?? '').trim());
}

/** `rules` and their nested blocks, as the declarations they apply to `element`. */
function appliedTo(rules: CSSRuleList, element: Element): string {
  const applied: string[] = [];
  const missed: string[] = [];
  for (const rule of rules) {
    // A style rule carries a selector; a grouping rule carries the rules
    // written inside it, which is where a rule a media query guards lives.
    // The order matters: a style rule carries both in the current spec, and
    // recursing into its own empty list would read no declaration at all.
    if ('selectorText' in rule) {
      const { selectorText } = rule as CSSStyleRule;
      try {
        if (element.matches(selectorText)) applied.push(rule.cssText);
      } catch {
        missed.push(selectorText);
      }
      continue;
    }
    const nested = 'cssRules' in rule ? (rule as CSSGroupingRule).cssRules : null;
    if (nested !== null) applied.push(appliedTo(nested, element));
  }
  expect(missed, `selectors jsdom could not read: ${missed.join(', ')}`).toEqual([]);
  return applied.join('\n');
}

describe('a long line in the conversation', () => {
  it('wraps a code line inside its panel rather than scrolling it', () => {
    const applied = declarationsFor(PANEL, '.code pre');

    expect(applied, 'the panel has a rule at all').not.toBe('');
    expect(valuesOf(applied, 'white-space'), 'the line wraps where the panel ends').toEqual([
      'pre-wrap',
    ]);
    expect(
      valuesOf(applied, 'overflow-wrap'),
      'a token with nowhere to break ends at the edge',
    ).toEqual(['anywhere']);
    expect(applied, 'so the panel is not a scroller').not.toContain('overflow-x');
  });

  it('wraps a diff line, keeping its indent, and breaks a token that cannot wrap', () => {
    const applied = declarationsFor(CALL, '.dif .ln.a .l');

    expect(applied, 'the line has a rule at all').not.toBe('');
    expect(valuesOf(applied, 'white-space'), 'the line wraps where the row ends').toEqual([
      'pre-wrap',
    ]);
    expect(
      valuesOf(applied, 'overflow-wrap'),
      'a token with nowhere to break ends at the edge',
    ).toEqual(['anywhere']);
  });

  it('leaves the diff box itself unable to scroll sideways', () => {
    const applied = declarationsFor(CALL, '.dif');

    expect(applied, 'the box has a rule at all').not.toBe('');
    expect(applied, 'nothing about it scrolls').not.toContain('overflow-x');
  });

  it('wraps and breaks a command result that has no space to wrap at', () => {
    const applied = declarationsFor(CALL, '.term');

    expect(applied, 'the result box has a rule at all').not.toBe('');
    expect(valuesOf(applied, 'white-space'), 'the result wraps where the box ends').toEqual([
      'pre-wrap',
    ]);
    expect(
      valuesOf(applied, 'overflow-wrap'),
      'a token with nowhere to break ends at the edge',
    ).toEqual(['anywhere']);
    expect(applied, 'so the result box is not a scroller').not.toContain('overflow-x');
  });

  it('breaks a token in a table cell so the table fits the column', () => {
    const applied = declarationsFor(TABLE, '.prose td');

    expect(applied, 'the cell has a rule at all').not.toBe('');
    expect(valuesOf(applied, 'overflow-wrap'), 'a cell with one long token still fits').toEqual([
      'anywhere',
    ]);
  });

  it('leaves a table header its own word, so no column can slice one', () => {
    // A header that may break anywhere has a one-character minimum, and a
    // neighbouring column's demand for width then slices `Verb` into `Ve` /
    // `rb`. Left to the browser's own default the column can never be narrower
    // than the header's longest word - and `nowrap` is not the way to say that:
    // a header of several words then cannot wrap, and the table is pushed past
    // the column instead.
    const applied = declarationsFor(TABLE, '.prose th');

    expect(applied, 'the header has a rule at all').not.toBe('');
    expect(valuesOf(applied, 'overflow-wrap'), 'a header word is never sliced').toEqual([]);
    expect(valuesOf(applied, 'white-space'), 'and the header may wrap at its spaces').toEqual([]);
  });

  it('leaves the sheet no sideways scroller at all', () => {
    // Every one of these was a scroller or a clip before the change, and the
    // smaller block that wrapped a diff line only below 560px was deleted with
    // them: a wrapping rule that has to be repeated inside a media query is a
    // scroller waiting to come back at the width nobody tests.
    expect(sheet, 'the sheet is the one this test read').toContain('.dif .ln .l');
    expect(sheet.replace(/\/\*[\s\S]*?\*\//g, ''), 'no rule scrolls a box sideways').not.toContain(
      'overflow-x',
    );
  });
});
