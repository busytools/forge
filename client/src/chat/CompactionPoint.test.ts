import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import CompactionPoint from './CompactionPoint.svelte';
import { tokens } from './numbers';

/** The row drawn open, which is the state its body needs. */
const draw = (
  trigger: string | null,
  preTokens: number | null,
  postTokens: number | null,
  summary: string | null = null,
  open = true,
): string =>
  render(CompactionPoint, { props: { trigger, preTokens, postTokens, summary, open } }).body;

const PAGE = readFileSync(
  new URL('../../../docs/book/src/ui/client/web-session.html', import.meta.url),
  'utf8',
);

/**
 * The rendered row's own words, with the tags and the markers Svelte leaves
 * between blocks taken out. A sentence is read as a sentence: the markup's
 * `{#if}` blocks are pieces of one line, and asserting on the pieces would pin
 * where the template's branches sit rather than what the reader is told.
 *
 * A tag stands in for a space - right between two words, wrong where it closed
 * over a body and the punctuation follows the word itself: the body keeps its
 * comma tight (`cut,`), so a tag directly before punctuation is skipped rather
 * than spaced. A real space there is no tag and survives, which is what keeps a
 * typographic slip from hiding behind this.
 */
const words = (html: string): string =>
  html
    .replace(/<!--[\s\S]*?-->/g, '')
    .replace(/<[^>]+>(?=[,.])/g, '')
    .replace(/<[^>]+>/g, ' ')
    .replace(/\s+/g, ' ')
    .trim();

describe('the compaction point a boundary leaves in the conversation', () => {
  /**
   * A closed point carries its summary and nothing else: the account behind
   * the cut is the body's.
   */
  it('carries summary markup only while it is closed', () => {
    const text = 'the compacted account';
    // No `open` prop: the production default, which is what a reader meets.
    const closed = render(CompactionPoint, {
      props: { trigger: 'auto', preTokens: 12_000, postTokens: 3_000, summary: text },
    }).body;
    const under = closed.slice(closed.indexOf('</summary>'));
    expect(under, 'no body under a closed point').not.toContain(text);

    const open = draw('auto', 12_000, 3_000, text);
    expect(open, 'and the account is drawn onto the open').toContain(text);
  });

  it('draws a hairline across the column, carrying the word and closed by default', () => {
    const body = draw('auto', 68_031, 9_149, null, false);

    expect(body, 'the row the approved shape draws').toContain('<details class="cpoint">');
    expect(body, 'collapsed, so the boundary is a hint rather than a block').not.toContain(
      'open=""',
    );
    // The hairline is two rules flanking the label, which is what makes the row
    // read across the column rather than as one more work row.
    expect(body.match(/class="rule"/g), 'one rule on each side of the word').toHaveLength(2);
    expect(body, 'the word the shape carries').toContain('class="word">compaction<');
    expect(body.match(/#i-chev/g), 'one disclosure chevron').toHaveLength(1);
  });

  it('carries the count before the cut short, and the whole figure behind it', () => {
    const body = draw('auto', 68_031, 9_149);

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
    const body = draw('auto', 1_016_576, 31_253);

    expect(body, 'short on the row, as every other figure on the page draws it').toContain(
      'class="n">1016.6k before<',
    );
    expect(words(body), 'with the whole figure behind it').toContain(
      'trigger auto \u{b7} 1,016,576 tokens read before the cut',
    );
  });

  it('says what the cut carried after it, when the frame held it', () => {
    // The CLI sends the post-compaction count in the same frame as the two
    // facts the row already stated, and forge's decode used to drop it.
    const body = draw('auto', 68_031, 9_149);

    expect(words(body), 'the sentence the body reads as').toContain(
      'trigger auto \u{b7} 68,031 tokens read before the cut, 9,149 carried after it',
    );
    expect(body, 'with the figure lifted out of the words, as its neighbours are').toContain(
      '<b>9,149</b> carried after it',
    );
  });

  it('says only what it was given when a boundary carried no post count', () => {
    // The state an older transcript or a rename inside the metadata leaves:
    // two of the three facts survive, and the row draws the ones it has.
    const body = draw('auto', 68_031, null);

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
    const body = draw(null, null, null, null, false);

    expect(body, 'the row still draws').toContain('<details class="cpoint">');
    expect(body, 'with the word it is named by').toContain('class="word">compaction<');
    expect(body, 'and no handle onto a body that does not exist').not.toContain('#i-chev');
    expect(body, 'and no count it was not given').not.toContain('class="n"');
  });

  /**
   * A rename inside the metadata keeps some facts and loses the rest, and the
   * body's clauses are independent - the pre-cut count's own drift is the one
   * the primitives test calls plausible, and it leaves the trigger behind.
   */
  it('draws a boundary that kept some of its facts', () => {
    const triggerOnly = draw('auto', null, null);
    expect(triggerOnly, 'a handle, because there is a body to open').toContain('#i-chev');
    expect(triggerOnly, 'and no count it was not given').not.toContain('class="n"');
    expect(words(triggerOnly), 'with the trigger alone on the body').toContain('trigger auto');
    expect(words(triggerOnly), 'and no clause about a count it lacks').not.toContain('tokens read');

    const countOnly = draw(null, 68_031, null);
    expect(countOnly, 'the count it was given').toContain('class="n">68.0k before<');
    expect(countOnly, 'a handle').toContain('#i-chev');
    expect(words(countOnly), 'the whole figure in the body').toContain(
      '68,031 tokens read before the cut',
    );
    expect(words(countOnly), 'and no trigger it does not have').not.toContain('trigger');

    // And the third mixed body, which only the carried-after clause produces:
    // the trigger and the count the cut carried, with the count before it gone.
    const triggerAndCarried = draw('auto', null, 9_149);
    expect(words(triggerAndCarried), 'the figure separated from the trigger it follows').toContain(
      'trigger auto, 9,149 carried after it',
    );
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

    // The carried-after clause, matched against the row the code draws at the
    // drawing's own numbers: the page and the component are two copies of one
    // sentence, and a wording or a figure changed in one and not the other is
    // how the drawing goes stale with every test still green.
    for (const [trigger, before, after] of [
      ['auto', 154_013, 4_712],
      ['manual', 88_412, 2_703],
    ] as const) {
      const clause =
        /<b>[\d,]+<\/b> carried after it/.exec(draw(trigger, before, after))?.[0] ?? '';
      expect(clause, 'the row the code draws carries the clause').not.toBe('');
      expect(PAGE, 'and the book draws the same one').toContain(clause);
    }
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

  it('opens onto the facts and the continuation summary under them', () => {
    // The summary is the one account of what the cut dropped, and it arrives
    // as its own frame: the row carries it rendered, under the facts.
    const body = draw(
      'auto',
      68_031,
      9_149,
      'This session is being continued from a previous conversation that ran out of context.\n\n## What happened\n\n- the lanes rework landed',
    );

    expect(words(body), 'the facts still draw as they did').toContain('trigger auto');
    expect(body, 'the summary is a heading, not raw text').toContain('<h2>');
    expect(body, 'with its list').toContain('<li>');
    expect(words(body), 'and its own lead sentence').toContain(
      'This session is being continued from a previous conversation',
    );
  });

  it('opens for a summary alone when the boundary carried no facts', () => {
    // A cut whose frame never reached the fold still happened; the handle
    // stays because the summary behind it is worth opening.
    const body = draw(null, null, null, 'This session is being continued from a previous one.');

    expect(body, 'the row still opens').toContain('<details class="cpoint"');
    expect(body, 'on the summary').toContain('This session is being continued');
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
