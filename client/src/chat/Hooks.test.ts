import { readFileSync } from 'node:fs';

import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import Hooks from './Hooks.svelte';
import type { HookInfo } from './units';

const ONE: HookInfo[] = [{ command: 'echo fixture-stop-hook-ok', durationMs: 3 }];

const draw = (actions: number, infos: HookInfo[] = ONE): string =>
  render(Hooks, { props: { actions, infos } }).body;

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
 * `.hooks` or `details.hooks` - rather than the live panel's `div.hooks` or
 * something inside it.
 */
function chipBox(selector: string): boolean {
  const bare = selector
    .trim()
    .replace(/::?[\w-]+(?:\([^)]*\))?/g, '')
    .replace(/\[[^\]]*\]/g, '');
  return /(^|[\s>+~])(?:details\.)?\.hooks$/.test(bare);
}

/** The values of `display` that lay a box's own children out in one row. */
const ROW_DISPLAYS = ['flex', 'inline-flex', 'grid', 'inline-grid'];

/** Both sheets that draw the chip: the app's own, and the book's copy of it. */
const SHEETS: [string, string][] = [
  ['web.css', SHEET],
  ['the book drawing', BOOK],
];

describe('the hook chip a turn carries', () => {
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

    const laying = SHEETS.flatMap(([where, sheet]) =>
      rules(sheet)
        .filter((rule) => rule.selectors.some(chipBox))
        .flatMap((rule) =>
          [...rule.body.matchAll(/display\s*:\s*([^;]+)/g)]
            .map(([, value = '']) => value.trim())
            .filter((value) => ROW_DISPLAYS.includes(value))
            .map((value) => `${where}: display: ${value}`),
        ),
    );

    expect(laying, `the chip's own box lays its children in a row: ${laying.join(', ')}`).toEqual(
      [],
    );
  });

  /**
   * The book's page is the drawing the surface is judged by, and it drew the
   * chip only closed - which is a blind spot for this defect, whose rows are
   * not on the page in that state at all.
   */
  it('draws the chip open in the book, where its rows can be seen', () => {
    const open = /<details class="hooks" open>[\s\S]*?<\/details>/.exec(PAGE)?.[0];
    expect(open, 'the book draws the chip open').toBeDefined();
    expect(open, 'with the rows it counted').toContain('class="body"');
    expect(open, 'each drawn as a command with its duration').toContain('class="term"');
  });
});
