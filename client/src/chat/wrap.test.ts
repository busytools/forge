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
 * **The rules are matched against the components' own markup, composed where
 * the page composes them.** A fragment rendered bare is a fixture that cannot
 * see an ancestor-scoped rule, and `.conv` is this sheet's own idiom for a rule
 * about the conversation: `.prose .code pre { white-space: pre }` would pass a
 * bare panel and silently clip a real one. Every rule in the parsed sheet is
 * tried against the element with `matches`, wherever it is written.
 *
 * The terminal is the reference here (rule 24): it wraps a long code line
 * inside its panel, wraps a diff line and re-emits the indent on the
 * continuation row, and hard-splits a token with nowhere to break
 * (`wrap_long_token` in `crates/forge-tui/src/ui/wrap.rs`).
 */
const sheet = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');

/**
 * Where the page puts a conversation fragment: the containers it hangs in, as
 * `Session.svelte`, `Chat.svelte` and `Turn.svelte` mount them.
 *
 * **The two unclassed wrappers between `.conv` and `.turn` are `virtua`'s**, so
 * a fixture without them matches a child combinator the page cannot:
 * `.conv > .turn .dif .ln.a .l` would red a rule that changes nothing real.
 * `turn` is the block the fragment sits in - the work a turn did, or the
 * reader's own message, which mounts its prose under `.mine`.
 */
const inPage = (fragment: string, turn: 'work' | 'mine' = 'work'): string =>
  `<div class="app"><main class="chat"><div class="conv"><div><div class="turn">` +
  `<div class="${turn}">${fragment}</div></div></div></div></main></div>`;

/** A call carrying a diff and a command's own output, as the fold hands it over. */
const CALL = inPage(
  render(Call, {
    props: {
      k: 'toolu_01',
      call: {
        id: 'toolu_01',
        row: { kind: 'family', family: 'edit' },
        name: 'Edit',
        title: 'Edit src/lib.rs',
        command: null,
        status: 'completed',
        note: null,
        body: [
          { kind: 'diff', old: 'let a = 1;', new: 'let a = 2;' },
          { kind: 'text', text: 'ok' },
        ],
        mutation: null,
        skill: null,
        image: null,
        imageNote: null,
      } as ToolLeaf,
    },
  }).body,
);

/** A source file in a message's prose, which is how a fence draws one. */
const PANEL = inPage(
  `<div class="prose">${render(Code, { props: { path: 'src/lib.rs', text: 'let a = 1;' } }).body}</div>`,
);

/** A message's prose, with a table in it, as the markdown renderer draws both. */
const PROSE = inPage(
  render(Prose, {
    props: {
      text: 'A paragraph with one_long_token_inside_it.\n\n| a | b |\n| - | - |\n| c | d |',
    },
  }).body,
);

/** The same, in the reader's own message, which `Turn.svelte` mounts `.mine`. */
const MINE = inPage(
  render(Prose, { props: { text: 'A paragraph with one_long_token_inside_it.' } }).body,
  'mine',
);

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

/** `rules` and their nested blocks, as the declarations they apply to `element`. */
function appliedTo(rules: CSSRuleList, element: Element): string {
  const applied: string[] = [];
  const missed: string[] = [];
  for (const rule of rules) {
    const children = 'cssRules' in rule ? (rule as CSSGroupingRule).cssRules : null;
    if ('selectorText' in rule) {
      const { selectorText } = rule as CSSStyleRule;
      // A nested rule is written with `&`, which no element matches and which
      // throws nothing: it is a declaration this reader cannot place, so it
      // fails loudly rather than reading as absent.
      if (children !== null && children.length > 0) {
        missed.push(selectorText);
        continue;
      }
      try {
        if (element.matches(selectorText)) applied.push(rule.cssText);
      } catch {
        missed.push(selectorText);
      }
      continue;
    }
    // A grouping rule carries what is written inside it, which is where a rule
    // a media query guards lives.
    if (children !== null) applied.push(appliedTo(children, element));
  }
  expect(missed, `selectors this reader could not place: ${missed.join(', ')}`).toEqual([]);
  return applied.join('\n');
}

/**
 * The values `declarations` gives `property`, duplicates collapsed.
 *
 * **Presence is not the question.** A rule that decides the other way collects
 * beside the one this file names - a second copy inside a media query, or a
 * selector with more specificity than it - and the element then reads whatever
 * the cascade says, not whatever the first rule found says.
 */
function valuesOf(declarations: string, property: string): string[] {
  const set = new RegExp(`(?:^|[;{\\s])${property}:\\s*([^;}]+)`, 'g');
  const found = [...declarations.matchAll(set)].map((match) => (match[1] ?? '').trim());
  return [...new Set(found)].sort();
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
    const applied = declarationsFor(PROSE, '.prose td');

    expect(applied, 'the cell has a rule at all').not.toBe('');
    expect(valuesOf(applied, 'overflow-wrap'), 'a cell with one long token still fits').toEqual([
      'anywhere',
    ]);
  });

  it('breaks a long token in prose, which is what carries a paragraph', () => {
    // A paragraph wraps at its spaces and not inside a word, so a path, a hash
    // or a URL in a sentence would take the conversation sideways on its own.
    // The paragraph declares nothing: `overflow-wrap` inherits, so the block is
    // the element that has to carry it, and the paragraph is the element that
    // has to leave it alone. Both of a turn's prose blocks, because they hang
    // in different containers - the work under `.work`, the reader's own under
    // `.mine` - and a rule scoped to either one is a rule about one of them.
    for (const [where, html] of [
      ['the work a turn did', PROSE],
      ["the reader's own message", MINE],
    ] as const) {
      const paragraph = declarationsFor(html, '.prose p');
      const block = declarationsFor(html, '.prose');

      expect(
        valuesOf(paragraph, 'overflow-wrap'),
        `${where}: the paragraph declares nothing of its own`,
      ).toEqual([]);
      expect(
        valuesOf(block, 'overflow-wrap'),
        `${where}: and inherits the break from the block`,
      ).toEqual(['anywhere']);
    }
  });

  it('leaves a table header its own word, so no column can slice one', () => {
    // A header that may break anywhere has a one-character minimum, and a
    // neighbouring column's demand for width then slices `Verb` into `Ve` /
    // `rb`. So the header resets what it would otherwise inherit from the prose
    // block - and `nowrap` is not the way to say that: a header of several
    // words then cannot wrap, and the table is pushed past the column instead.
    const applied = declarationsFor(PROSE, '.prose th');

    expect(applied, 'the header has a rule at all').not.toBe('');
    expect(valuesOf(applied, 'overflow-wrap'), 'a header word is never sliced').toEqual(['normal']);
    expect(valuesOf(applied, 'white-space'), 'and the header may wrap at its spaces').toEqual([]);
  });

  it('leaves the sheet no sideways scroller at all', () => {
    // Every one of these was a scroller or a clip before the change, and the
    // smaller block that wrapped a diff line only below 560px was deleted with
    // them: a wrapping rule that has to be repeated inside a media query is a
    // scroller waiting to come back at the width nobody tests.
    expect(sheet, 'the sheet is the one this test read').toContain('.dif .ln .l');
    const code = sheet.replace(/\/\*[\s\S]*?\*\//g, '');
    // The shorthand is the same defect, since it scrolls both axes; an
    // `overflow-y` alone is not, and the rail and the conversation use it.
    const scrollers = [...code.matchAll(/overflow(?!-y)(?:-x)?:\s*(auto|scroll)/g)].map(
      (match) => match[0],
    );
    expect(scrollers, 'no rule scrolls a box sideways').toEqual([]);
  });
});
