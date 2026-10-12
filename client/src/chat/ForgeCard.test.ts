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

/** The heading each sheet opens the block with. */
const HEAD = "/* ---------- a forge call's own card ----------";

/**
 * The block's own text out of a sheet, from its heading to the NEXT section
 * heading - whichever one that is.
 *
 * **Not "up to the chevron heading"**, which is what this read first and what
 * a merge broke: main landed the models page's rules between this block and
 * the chevron's, so the slice swallowed them and the count went to 94. The
 * block ends where the next one starts, and what that next one is called is
 * not this pin's business.
 */
function blockOf(sheet: string, head: string): string {
  const from = sheet.indexOf(head);
  if (from === -1) return '';
  const next = sheet.indexOf('/* ---------- ', from + head.length);
  return sheet.slice(from, next === -1 ? undefined : next);
}

const SHEET_BLOCK = blockOf(SHEET, `${HEAD} */`);
const BOOK_BLOCK = blockOf(PAGE, HEAD);

/** One rule's selector and its declarations, whitespace flattened. */
interface Rule {
  selector: string;
  body: string;
}

/**
 * Every `selector { declarations }` in a block, comments out and the rules
 * sorted by selector.
 *
 * **Sorted rather than in order, and that is a deliberate divergence from
 * `Hook.test.ts`'s order-sensitive pin.** Ordering is not free there and it
 * is here: 126 pairs of these selectors carry equal specificity and do share
 * properties, so a reorder COULD decide a rendering in principle - but no two
 * of them can match one element (a summary cannot be both the bare chip and
 * its `.info` arm, nor a list row both its state and its when), so no order
 * among them is observable on this block. What this pin is for is a rule whose
 * DECLARATIONS drifted from the sheet's, which sorting catches exactly; a
 * moved rule is the case it gives up, and the drawing's own specimens are
 * where that would show.
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

  it('renders a quote as markdown, so a spawn charter draws as it was written', () => {
    // The charter is written in markdown and drew raw before (Ved, 2026-10-09);
    // the quote piece is prose wherever it comes from, so it renders.
    const drawn = draw({
      pieces: [
        { kind: 'tag', text: 'charter' },
        { kind: 'quote', text: '## Steps\n\n- **first** the run, then `code`.' },
      ],
    });

    expect(drawn, 'the quote draws').toContain('fam-quote');
    expect(drawn, 'the heading does not draw as its hashes').not.toContain('## Steps');
    expect(drawn, 'a bold run renders').toContain('<strong>first</strong>');
    expect(drawn, 'and code renders').toContain('<code>code</code>');
  });

  /**
   * **A quote keeps the line breaks it was written with.**
   *
   * Most of them are the reader's own words - a draft, a review prompt - and
   * the terminal's own split is that a person's newlines survive where
   * generated prose joins back. Without it a two-line Slack draft draws as one
   * flowing paragraph, which is a second visible change from the markdown fix
   * and not the one it was for.
   */
  it('keeps a quote\u{27}s own line breaks, which are the reader\u{27}s', () => {
    const drawn = draw({
      pieces: [{ kind: 'quote', text: 'Deploy is done\nWill check the logs tomorrow' }],
    });

    expect(drawn, 'the reader\u{27}s own line break was joined away').toContain('<br>');
  });
});
