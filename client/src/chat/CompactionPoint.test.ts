import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import CompactionPoint from './CompactionPoint.svelte';
import { tokens } from './numbers';

/** The row, as a boundary's own frame fills it. */
const draw = (trigger: string | null, preTokens: number | null): string =>
  render(CompactionPoint, { props: { trigger, preTokens } }).body;

const PAGE = readFileSync(
  new URL('../../../docs/book/src/ui/client/web-session.html', import.meta.url),
  'utf8',
);

/**
 * The rendered row's own words, with the tags and the markers Svelte leaves
 * between blocks taken out. A sentence is read as a sentence: the markup's
 * `{#if}` blocks are pieces of one line, and asserting on the pieces would pin
 * where the template's branches sit rather than what the reader is told.
 */
const words = (html: string): string =>
  html
    .replace(/<!--[\s\S]*?-->/g, '')
    .replace(/<[^>]+>/g, ' ')
    .replace(/\s+/g, ' ')
    .trim();

describe('the compaction point a boundary leaves in the conversation', () => {
  it('draws a hairline across the column, carrying the word and closed by default', () => {
    const body = draw('auto', 68_031);

    expect(body, 'the row the approved shape draws').toContain('<details class="cpoint">');
    expect(body, 'collapsed, so the boundary is a hint rather than a block').not.toContain(
      '<details class="cpoint" open>',
    );
    // The hairline is two rules flanking the label, which is what makes the row
    // read across the column rather than as one more work row.
    expect(body.match(/class="rule"/g), 'one rule on each side of the word').toHaveLength(2);
    expect(body, 'the word the shape carries').toContain('class="word">compaction<');
    expect(body.match(/#i-chev/g), 'one disclosure chevron').toHaveLength(1);
  });

  it('carries the count before the cut short, and the whole figure behind it', () => {
    const body = draw('auto', 68_031);

    expect(body, 'a short count on the row itself').toContain('class="n">68.0k before<');
    expect(words(body), 'the trigger and the figure beside it, as one sentence').toContain(
      'trigger auto \u{b7} 68,031 tokens read before the cut',
    );
    // The values are lifted out of the sentence, which is the mock's own rule
    // for a figure inside a body.
    expect(body, 'the trigger lifted out of the words').toContain('<b>auto</b>');
    expect(body, 'and the figure with every digit').toContain('<b>68,031</b>');
  });

  /**
   * The row at the scale a real boundary headlines. Measured live, a
   * `pre_tokens` on a 1M-context session sits just over a million (`1016576`),
   * and the short form is the same shape the report row already draws a token
   * figure in - four digits and a `k` - so a millions form here would be a
   * second token format on one page.
   */
  it('draws the wide figure a 1M-context boundary headlines', () => {
    const body = draw('auto', 1_016_576);

    expect(body, 'short on the row, as every other figure on the page draws it').toContain(
      'class="n">1016.6k before<',
    );
    expect(words(body), 'with the whole figure behind it').toContain(
      'trigger auto \u{b7} 1,016,576 tokens read before the cut',
    );
  });

  it('says nothing about what was carried after the cut', () => {
    // The CLI's frame carries a post-compaction count and forge's decode drops
    // it, so no client frame has ever held one. A body carrying that sentence
    // is the row reading a fact nothing can fill.
    const body = draw('auto', 68_031);

    expect(words(body), 'the body ends at the count it was given').toContain(
      'tokens read before the cut',
    );
    expect(words(body), 'with no carried-after clause').not.toContain('carried after');
    expect(body, 'and no figure standing in for one').not.toMatch(/<b>[\d.,k]+<\/b> carried/);
  });

  it('draws a bare boundary without a handle, because it has nothing to open', () => {
    // The bare shape is drift: the outer key renamed leaves the frame in the
    // CLI's generic bucket with no metadata anywhere in it. The row is the
    // compaction, and a chevron promising a body would open onto nothing.
    const body = draw(null, null);

    expect(body, 'the row still draws').toContain('<details class="cpoint">');
    expect(body, 'with the word it is named by').toContain('class="word">compaction<');
    expect(body, 'and no handle onto a body that does not exist').not.toContain('#i-chev');
    expect(body, 'and no count it was not given').not.toContain('class="n"');
  });

  /**
   * A rename inside the metadata keeps one fact and loses the other, and the
   * two clauses of the body are independent - the count's own drift is the one
   * the primitives test calls plausible, and it leaves the trigger behind.
   */
  it('draws a boundary that kept one of its two facts', () => {
    const triggerOnly = draw('auto', null);
    expect(triggerOnly, 'a handle, because there is a body to open').toContain('#i-chev');
    expect(triggerOnly, 'and no count it was not given').not.toContain('class="n"');
    expect(words(triggerOnly), 'with the trigger alone on the body').toContain('trigger auto');
    expect(words(triggerOnly), 'and no clause about a count it lacks').not.toContain('tokens read');

    const countOnly = draw(null, 68_031);
    expect(countOnly, 'the count it was given').toContain('class="n">68.0k before<');
    expect(countOnly, 'a handle').toContain('#i-chev');
    expect(words(countOnly), 'the whole figure in the body').toContain(
      '68,031 tokens read before the cut',
    );
    expect(words(countOnly), 'and no trigger it does not have').not.toContain('trigger');
  });

  /**
   * The book page is the drawing this surface is judged by. The specimen shapes
   * the mockup carried are out of it; what holds is that the real row is drawn
   * where a boundary sits, in each state the code has.
   */
  it('draws the row in the book, where a boundary sits', () => {
    const rows = PAGE.match(/<details class="cpoint"(?: open)?>[\s\S]*?<\/details>/g) ?? [];
    expect(rows.length, 'the drawing carries the row').toBeGreaterThanOrEqual(2);

    const row = rows.find((candidate) => candidate.startsWith('<details class="cpoint">')) ?? '';
    expect(row, 'the book draws the row rather than leaving the shape to prose').not.toBe('');
    expect(row, 'carrying the column-wide hairline').toContain('class="rule"');
    expect(row, 'the word').toContain('class="word">compaction<');
    expect(row, 'and the count before the cut').toMatch(/class="n">[\d.]+k before</);

    // The open state too, since the body is the half a reader judges - and the
    // half the row could read a fact nothing fills.
    const open = rows.find((candidate) => candidate.includes('"cpoint" open')) ?? '';
    expect(open, 'and draws it open once, so the body is on the page').toContain('class="cpbody"');

    // And the third state, which the code draws and the page would otherwise
    // leave out: a frame that carried no metadata at all.
    const bare = rows.find((candidate) => !candidate.includes('class="cpbody"')) ?? '';
    expect(bare, 'the page draws the bare boundary too').not.toBe('');
    expect(bare, 'with no count it was not given').not.toContain('class="n"');
    expect(bare, 'and no handle onto a body that does not exist').not.toContain('#i-chev');
  });

  /**
   * The two figures a drawn row carries are one number, and nothing else holds
   * them together: the summary's short count and the body's grouped one are
   * both written by hand in the page, and a reader comparing them is how a
   * stale pair would be found.
   */
  it('draws the same figure short on the row and whole in its body', () => {
    const countable = (
      PAGE.match(/<details class="cpoint"(?: open)?>[\s\S]*?<\/details>/g) ?? []
    ).filter((row) => row.includes('class="cpbody"'));
    expect(countable.length, 'the drawn rows that carry a count').toBeGreaterThanOrEqual(2);

    for (const row of countable) {
      const short = /class="n">([^<]+) before</.exec(row)?.[1] ?? '';
      const whole = /<b>([\d,]+)<\/b> tokens read before the cut/.exec(row)?.[1] ?? '';
      expect(whole, 'the body states the count it was given').not.toBe('');
      expect(short, 'and the row states the same one, drawn the way a row draws it').toBe(
        tokens(Number(whole.replaceAll(',', ''))),
      );
    }
  });

  /**
   * The rules ship in the app's own sheet, beside the other chat rows, and the
   * drawing keeps its own copy because it has to stand alone. Two copies, so
   * the three rules that define the row - the hairline, the hover lift and the
   * open accent - are pinned in both: editing one copy and not the other is how
   * the row loses its hairline with every test still green.
   */
  it('draws the row from the app sheet and the drawing alike', () => {
    const app = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');
    const book = /<style>([\s\S]*?)<\/style>/.exec(PAGE)?.[1] ?? '';

    for (const [where, css] of [
      ['web.css', app],
      ['the book drawing', book],
    ] as const) {
      // The extraction's own denominator: a sheet read as empty would report
      // every rule missing, and a regex over a page can read nothing at all.
      expect(css.length, `${where}, read rather than empty`).toBeGreaterThan(1000);
      expect(css, `${where} draws the hairline across the column`).toMatch(
        /\.cpoint \.rule \{[^}]*flex: 1/,
      );
      expect(css, `${where} lifts the word on hover`).toMatch(
        /\.cpoint:hover \.word \{[^}]*color: var\(--text\)/,
      );
      expect(css, `${where} and turns it to the accent on opening`).toMatch(
        /details\.cpoint\[open\] > summary \.word \{[^}]*color: var\(--accent\)/,
      );
    }
  });
});
