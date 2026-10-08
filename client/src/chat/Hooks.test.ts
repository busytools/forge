import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import Hooks from './Hooks.svelte';
import type { HookInfo } from './units';

const ONE: HookInfo[] = [{ command: 'echo fixture-stop-hook-ok', durationMs: 3 }];

/** The chip drawn open, which is the state its body needs. */
const draw = (
  actions: number,
  infos: HookInfo[] = ONE,
  errors: string[] = [],
  open = true,
): string => render(Hooks, { props: { actions, infos, errors, open } }).body;

/**
 * The chip's own `<summary>`. The closed-chip state has to live here: a mark
 * or a count drawn anywhere inside the open body only is a state a reader
 * sees after opening the chip, which is the blind spot the failed state
 * exists to close.
 */
const summaryOf = (body: string): string =>
  body.slice(body.indexOf('<summary'), body.indexOf('</summary>'));

const SHEET = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');
const PAGE = readFileSync(
  new URL('../../../docs/book/src/ui/client/web-session.html', import.meta.url),
  'utf8',
);

/** The book page's own copy of the rules it draws with. */
const BOOK = /<style>([\s\S]*?)<\/style>/.exec(PAGE)?.[1] ?? '';

/** Every `selector { declarations }` rule in a sheet, its selector list kept whole. */
const rules = (sheet: string): { selectors: string[]; body: string }[] =>
  [...sheet.matchAll(/([^{}]+)\{([^{}]*)\}/g)].map(([, selectors = '', body = '']) => ({
    selectors: selectors.split(','),
    body,
  }));

/**
 * Whether a selector's last compound is the chip's own `<details>` box - bare
 * `.hooks` or `details.hooks` - rather than something inside it, or a chip
 * drawn as some other element.
 */
function chipBox(selector: string): boolean {
  const bare = selector
    .trim()
    .replace(/::?[\w-]+(?:\([^)]*\))?/g, '')
    .replace(/\[[^\]]*\]/g, '');
  return /(^|[\s>+~])(?:details)?\.hooks$/.test(bare);
}

/** The values of `display` that lay a box's own children out in one row. */
const ROW_DISPLAYS = ['flex', 'inline-flex', 'grid', 'inline-grid'];

/** Both sheets that draw the chip: the app's own, and the book's copy of it. */
const SHEETS: [string, string][] = [
  ['web.css', SHEET],
  ['the book drawing', BOOK],
];

describe('the hook chip a turn carries', () => {
  /**
   * A closed chip carries its summary and nothing else: the commands are the
   * body's, and the giant seat's cost is the bodies of rows nobody opened.
   */
  it('carries summary markup only while it is closed', () => {
    const closed = draw(1, ONE, [], false);
    const under = closed.slice(closed.indexOf('</summary>'));
    expect(under, 'no body under a closed chip').not.toContain('echo fixture-stop-hook-ok');

    const open = draw(1);
    expect(open, 'and the commands are drawn onto the open').toContain('echo fixture-stop-hook-ok');
  });

  it('counts one hook in the singular', () => {
    expect(draw(1), 'the count the captured row carries').toContain('1 action<');
    expect(draw(2)).toContain('2 actions<');
  });

  it('carries the disclosure chevron the rest of the column draws, and no spelled-out verb', () => {
    // Every other `<details>` here leads its toggle with the shared chevron,
    // so this chip spelling `expand` instead is a second convention for one
    // job. The chip keeps its own words either way, which is what a reader
    // who cannot see the glyph is told the disclosure is.
    const body = draw(1);

    // One chevron, and the one the stylesheet's open/closed rules reach. The
    // chip used to lead with a chevron that meant nothing by its state, so a
    // second one beside it would be two answers to the same question.
    expect(body.match(/#i-chev/g), 'exactly one disclosure chevron').toHaveLength(1);
    expect(body, 'and it is the shared one the stylesheet turns').toContain('class="ic arw"');
    expect(body, 'and not the verb spelled out in its place').not.toMatch(
      />\s*(expand|collapse)\s*</,
    );
  });

  /**
   * A state visible only when the chip is open is where the chip's last defect
   * lived, so the failure has to reach the closed chip: the mark every other
   * failure on the page leads with, and the count of what the CLI reported.
   */
  it('marks the summary failed, and counts the errors, when the CLI reported any', () => {
    const failed = summaryOf(draw(1, ONE, ['JSON validation failed']));

    expect(failed, 'the failure mark a failed call, turn or run leads with').toContain('#i-x');
    expect(failed, 'and it is drawn as the shared state mark').toContain('class="ic st err"');
    expect(failed, 'with the count beside the action count').toContain('1 error<');
    expect(
      summaryOf(draw(1, ONE, ['one', 'two'])),
      'plural when more than one came back',
    ).toContain('2 errors<');

    const clean = summaryOf(draw(1));
    expect(clean, 'a summary with no errors carries no mark').not.toContain('#i-x');
    expect(clean, 'and says nothing about errors').not.toContain('error');
  });

  /**
   * The corpus' most common failing shape is more hooks than errors - the
   * errors name no hook - so the two draw as their own lists rather than an
   * error against a command.
   */
  it('draws each error as its own row under the hooks, in the bad tone', () => {
    const body = draw(
      2,
      [
        { command: 'bash hooks/check.sh', durationMs: 6 },
        { command: 'cargo fmt --check', durationMs: 412 },
      ],
      ['JSON validation failed'],
    );

    expect(body, 'the error as the row itself, in the tone the sheets give a failure').toContain(
      '<div class="term"><span class="fail">JSON validation failed</span></div>',
    );
    expect(body, 'both hooks still drawn, so neither list replaces the other').toContain(
      'cargo fmt --check',
    );

    expect(
      draw(1, [], ['JSON validation failed']),
      'and the errors draw even where the rows they came from carry none',
    ).toContain('<span class="fail">JSON validation failed</span>');
  });

  it('draws the hooks behind the count, in the unit their durations are in', () => {
    expect(draw(1)).toContain('echo fixture-stop-hook-ok');
    expect(draw(1), 'the duration the captured row records').toContain('3ms');
    expect(draw(1, [{ command: 'cargo fmt --check', durationMs: 1180 }])).toContain('1.2s');
  });

  /**
   * The chip is a summary with its rows under it: a row-laying display on the
   * `<details>` itself makes the summary and the body two items of ONE row, so
   * the rows draw beside the summary. jsdom performs no layout and cannot see
   * that, so the sheets' own rules are what there is to read.
   */
  it('draws the hook rows under the summary rather than beside it', () => {
    expect(draw(1), 'the chip the sheets style').toContain('<details class="hooks"');
    // The scan's own denominator: an extraction that reads nothing makes the
    // chip look clean while no rule was read at all, and this half is a
    // regex over a page rather than a file read.
    expect(BOOK, "the book page's own sheet, extracted rather than empty").toContain('.hooks');

    const laying = SHEETS.flatMap(([where, sheet]) => {
      const boxes = rules(sheet).filter((rule) => rule.selectors.some(chipBox));
      // The classifier's own denominator: a predicate that matches none of the
      // spellings it claims reports every sheet clean, so each sheet has to
      // name the chip's box before its silence means anything.
      expect(boxes.length, `${where} spells the chip's own rule`).toBeGreaterThan(0);
      return boxes.flatMap((rule) =>
        [...rule.body.matchAll(/display\s*:\s*([^;]+)/g)]
          .map(([, value = '']) => value.trim())
          .filter((value) => ROW_DISPLAYS.includes(value))
          .map((value) => `${where}: display: ${value}`),
      );
    });

    expect(laying, `the chip's own box lays its children in a row: ${laying.join(', ')}`).toEqual(
      [],
    );
  });

  /**
   * The classifier's spellings, named directly. Both sheets write the chip's
   * box rule as bare `.hooks` today, so a predicate that lost the
   * `details.hooks` spelling would classify them identically and the scan above
   * would never tell the two apart.
   */
  it('names both spellings of the chip box and nothing wider', () => {
    expect(chipBox('.hooks'), 'the bare box').toBe(true);
    expect(chipBox('details.hooks'), 'the box with its element named').toBe(true);
    expect(chipBox('.conv .hooks'), 'the box reached through an ancestor').toBe(true);
    expect(chipBox('.hooks .term'), 'a rule for something inside it').toBe(false);
    expect(chipBox('div.hooks'), 'a chip drawn as another element').toBe(false);
    expect(chipBox('.hooksy'), 'a name that merely starts the same').toBe(false);
  });

  /**
   * The book's page is the drawing the surface is judged by, and it drew the
   * chip only closed - which is a blind spot for this defect, whose rows are
   * not on the page in that state at all.
   */
  it('draws the chip open in the book, where its rows can be seen', () => {
    const open = /<details class="hooks" open>[\s\S]*?<\/details>/.exec(PAGE)?.[0] ?? '';
    expect(open, 'the book draws the chip open').not.toBe('');
    expect(open, 'with the rows it holds in the shared body').toContain('class="body"');

    // The sample rows are the drawing's own to re-word; what holds is the
    // SHAPE - one row per action the chip counted, each a command with the
    // duration it took.
    // The chip spells one action in the singular, so a one-action drawing is a
    // real rendering the page may hold.
    const counted = /hook summary &#183; (\d+) actions?/.exec(open)?.[1] ?? '';
    expect(counted, 'the chip says how many actions it took').not.toBe('');
    const rows = open.match(/<div class="term">[^<]+<\/div>/g) ?? [];
    expect(rows.length, 'a row drawn per action the chip counted').toBe(Number(counted));
    expect(
      rows.filter((row) => !/<div class="term">[^<]+ &#183; .*\d+(\.\d+)?(ms|s)/.test(row)),
      'every row a command with the duration it took',
    ).toEqual([]);
  });

  /**
   * The failed half the chip exists for, and a page drawing only the passing
   * chip is the same one-state blind spot the open drawing was added to close.
   * What holds is the SHAPE: the mark and the count on the closed chip, so the
   * failure shows without opening it, and the error as a row under the hooks.
   */
  it('draws the failed chip in the book, its state on the closed chip', () => {
    const chips = PAGE.match(/<details class="hooks" open>[\s\S]*?<\/details>/g) ?? [];
    const failed = chips.find((chip) => chip.includes('#i-x')) ?? '';
    expect(failed, 'the book draws the failed chip open').not.toBe('');

    const summary = failed.slice(failed.indexOf('<summary'), failed.indexOf('</summary>'));
    expect(summary, 'the mark on the closed chip').toContain('#i-x');
    expect(summary, 'in the class the sheets colour as a failure').toContain('class="ic st err"');
    expect(summary, 'and the count beside the action count').toContain(
      'hook summary &#183; 1 action &#183; 1 error',
    );
    expect(failed, 'the error as its own row, in the tone the sheets give a failure').toContain(
      '<div class="term"><span class="fail">',
    );

    // The selector alone proves nothing - it matches whatever declaration the
    // rule carries, and other rules on this page carry the same token - so
    // both halves are pinned together: dropping the mark's class or swapping
    // the token is what turns a failure green.
    expect(BOOK, "the page's own sheet colours the failed summary's mark").toMatch(
      /details\.hooks > summary \.st\.err \{[^}]*color: var\(--bad\)/,
    );
  });
});
