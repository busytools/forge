import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import Forge from './Forge.svelte';
import type { ForgeCard } from './forge';

/**
 * The forge card's body, as markup and as rules.
 *
 * The rules are read where the two sheets are, and compared the way
 * `Hook.test.ts` compares its row's: the drawing follows the sheet, rule for
 * rule. It did not, unnoticed, until this pin existed - thirteen of the
 * block's rules had drifted, which is what a hand-copied block does.
 */

const SHEET = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');
const PAGE = readFileSync(
  new URL('../../../docs/book/src/ui/client/web-session.html', import.meta.url),
  'utf8',
);

/** The heading each sheet opens the block with, and the one that ends it. */
const HEAD = "/* ---------- a forge call's own card ----------";
const NEXT = '/* ---------- the chevron arrives';

const SHEET_BLOCK = SHEET.slice(SHEET.indexOf(`${HEAD} */`), SHEET.indexOf(NEXT));
const BOOK_BLOCK = PAGE.slice(PAGE.indexOf(HEAD), PAGE.indexOf(NEXT));

/** One rule's selector and its declarations, whitespace flattened. */
interface Rule {
  selector: string;
  body: string;
}

/**
 * Every `selector { declarations }` in a block, comments out and the rules
 * sorted by selector.
 *
 * **Sorted rather than in order, unlike `Hook.test.ts`'s pin**, because the
 * order these rules sit in decides nothing: they are all distinct selectors,
 * so no two of them compete for one property at one specificity. What the pin
 * is for is a rule whose DECLARATIONS drifted from the sheet's, which sorting
 * catches exactly.
 *
 * A rule this walk leaves out is a rule the comparison passes over in BOTH
 * sheets, so the count it read is asserted where it is used.
 */
function rules(css: string): Rule[] {
  const code = css.replace(/\/\*[\s\S]*?\*\//g, '');
  const out: Rule[] = [];
  for (const held of code.split('}')) {
    const at = held.indexOf('{');
    if (at === -1) continue;
    const selector = held.slice(0, at).trim().replace(/\s+/g, ' ');
    if (selector === '' || selector.startsWith('@')) continue;
    out.push({
      selector,
      body: held
        .slice(at + 1)
        .trim()
        .replace(/\s+/g, ' '),
    });
  }
  return out.sort((left, right) => left.selector.localeCompare(right.selector));
}

/** A rule as one string, the way two sheets are compared. */
const asText = (rule: Rule): string => `${rule.selector} { ${rule.body} }`;

const card = (over: Partial<ForgeCard> = {}): ForgeCard => ({
  title: 'review #3 - 1 open',
  chips: [],
  figure: null,
  pieces: [],
  ...over,
});

const draw = (over: Partial<ForgeCard> = {}): string =>
  render(Forge, { props: { card: card(over), glyph: 'review' } }).body;

describe("the forge card's own rules", () => {
  it('mirrors the sheet in the drawing, rule for rule', () => {
    const app = rules(SHEET_BLOCK);
    // The denominator, twice: a block the scan failed to read compares equal
    // to an empty one, and so does one that lost the rules a reader sees.
    expect(app.length, 'the sheet spells the card').toBeGreaterThan(20);
    expect(
      app.filter((rule) => rule.selector.includes('fam-')).length,
      'and most of it is the card own names',
    ).toBeGreaterThan(20);

    const book = rules(BOOK_BLOCK);
    expect(
      book.length,
      'and the drawing spells the same number of rules, so one dropped there is not silence',
    ).toBe(app.length);
    expect(book.map(asText), 'and every one of them mirrors the sheet').toEqual(app.map(asText));
  });
});

describe('the forge card body', () => {
  it('draws a review comment as its own block, code and thread and all', () => {
    // The whole review-detail drawing is this arm: a comment's spot, the side
    // it sits on, its state, the lines it was filed against and the turns.
    const drawn = draw({
      pieces: [
        {
          kind: 'comments',
          items: [
            {
              where: 'src/chat/units.ts:919',
              side: 'new side',
              state: { text: 'open', tone: 'bad' },
              context: ["mcp__forge__agents__send_message: { body: 'message' },"],
              turns: [
                { author: 'you', text: 'Reading this fresh - is it thread-only?', you: true },
                { author: 'worker', text: 'Right - that is the shipped shape.', you: false },
              ],
            },
          ],
        },
      ],
    });

    expect(drawn, 'the block draws').toContain('fam-cm');
    expect(drawn, 'the spot it was filed on').toContain('src/chat/units.ts:919');
    expect(drawn, 'the side, in its own words').toContain('new side');
    expect(drawn, 'the state as a chip').toContain('fam-chip bad');
    expect(drawn, 'with its own word').toContain('open');
    expect(drawn, 'the code it was filed against').toContain('send_message');
    expect(drawn, 'the reviewer apart from the worker').toContain('fam-turn you');
    expect(drawn, 'and both turns in words').toContain('is it thread-only?');
    expect(drawn, 'the reply too').toContain('that is the shipped shape');
  });

  it('draws a text piece the card itself does not read', () => {
    // A result's later text block: `review__list` appends one naming the other
    // branches that have reviews, and the card replaces the body it came in.
    const drawn = draw({
      pieces: [{ kind: 'text', text: 'this project also has reviews on: work/x.' }],
    });

    expect(drawn, 'the block draws').toContain('fam-text');
    expect(drawn, 'with its own words').toContain('this project also has reviews on: work/x.');
  });
});
