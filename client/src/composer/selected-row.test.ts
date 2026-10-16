/**
 * The autocomplete popover's selected row, and the token that grounds it.
 *
 * **The defect behind this file.** `.ac .it.sel` drew its ground as the accent
 * at 9% - `background: rgba(244,118,0,.09)` - under a row whose detail text is
 * `--dim`. Composited, that tint measured 4.41:1 against the 4.5 text floor,
 * and no token-level check can carry it: `contrast.test.ts` measures tokens
 * against each other, and an rgba layer over a ground is neither of the two.
 * The row draws `var(--sel)` now, the flat ground the theme names for a
 * selected option row, whose pair in that table is `--dim` at 4.65:1.
 *
 * **Why the pin is here rather than in `contrast.test.ts`.** That file can see
 * the pair and not the rule: a restyle that puts the tint back leaves the
 * `--dim` on `--sel` row green, because a rule naming an rgba layer reaches no
 * pair at all. Which token a rule NAMES is this file's claim, the same split
 * `connect/controls.test.ts` makes for the connect screen's two controls.
 *
 * It reads the app's own sheet and the drawing's copy of the rule, because the
 * drawing is what the page is held against and the two are edited apart.
 */

import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

import { repoPath } from '../repo';

/** The rule this file pins, and the ground it has to name. */
const SELECTOR = '.ac .it.sel';
const GROUND = '--sel';

/**
 * Every `selector { declarations }` block in a sheet.
 *
 * Neither side of the pattern can cross a brace, so a rule nested inside an
 * at-rule is read and the at-rule's own header is not - which is the half the
 * census below counts.
 */
const RULE = /([^{}]+)\{([^{}]*)\}/g;

/** The at-rules whose body holds other blocks, so their `{` opens no rule. */
const AT_RULE_BLOCK = /@(media|keyframes|supports|container|layer|scope)\b[^{;]*\{/g;

function rules(sheet: string): { selector: string; body: string }[] {
  return [...sheet.matchAll(RULE)].map(([, selector = '', body = '']) => ({
    selector: selector.trim().replace(/\s+/g, ' '),
    body,
  }));
}

/** A sheet as these checks read it: comments stripped, so a brace in one is no block. */
function sheetText(raw: string): string {
  return raw.replace(/\/\*[\s\S]*?\*\//g, '');
}

/** Every `<style>` block in a drawing, joined rather than the first taken. */
function sheetOf(page: string): string {
  return sheetText(
    [...page.matchAll(/<style>([\s\S]*?)<\/style>/g)].map(([, css = '']) => css).join('\n'),
  );
}

/**
 * The value a sheet's own rule for `selector` declares for `background`, or
 * why it declares none - so a renamed selector or a rule that lost the
 * declaration reports itself rather than comparing `undefined` quietly.
 */
function groundIn(sheet: string, selector: string): string {
  const rule = rules(sheet).find((found) => found.selector === selector);
  if (rule === undefined) return 'no rule draws this selector';
  const declared = /(?:^|;)\s*background\s*:\s*([^;]+)/.exec(rule.body)?.[1]?.trim();
  return declared ?? 'the rule declares no background';
}

const SHEET = sheetText(readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8'));
const DRAWING = sheetOf(
  readFileSync(repoPath('docs/book/src/ui/client/web-composer.html'), 'utf8'),
);

const SHEETS: [string, string][] = [
  ['assets/web.css', SHEET],
  ['the composer drawing', DRAWING],
];

describe("the autocomplete's selected row", () => {
  /**
   * The denominator. The check below reads one selector out of each sheet, so
   * a parse that read a fraction of the file finds it and reports a clean,
   * plausible answer over everything it did not see. Every `{` in the sheet is
   * either a rule this parse read or the head of an at-rule block it named,
   * and the two counts are taken independently - one from the parse, one off
   * the raw text.
   */
  it('reads every block of each sheet', () => {
    for (const [where, sheet] of SHEETS) {
      expect(sheet.length, `${where} was read at all`).toBeGreaterThan(0);

      const blocks = [...sheet.matchAll(AT_RULE_BLOCK)].length;
      const parsed = rules(sheet).length;
      const opens = (sheet.match(/\{/g) ?? []).length;

      expect(parsed, `${where} carries rules to read`).toBeGreaterThan(0);
      expect(
        parsed + blocks,
        `${where}: the blocks this parse read and the blocks the sheet holds disagree, so the ` +
          'parse has stopped seeing a shape the sheet writes and read a smaller sheet than ' +
          'exists. Name it in AT_RULE_BLOCK if a nested at-rule was added; if none was, the ' +
          'rule pattern is what moved',
      ).toBe(opens);
    }
  });

  /**
   * One assertion per sheet, because the two are edited apart: a rule corrected
   * in the app's sheet and left in the drawing is a page that drew the defect
   * after the app stopped.
   */
  it('grounds the row in the token its detail text is measured against', () => {
    for (const [where, sheet] of SHEETS) {
      expect(groundIn(sheet, SELECTOR), `${where}: the selected row's ground`).toBe(
        `var(${GROUND})`,
      );
    }
  });

  /**
   * The check's own control: run against the shape the defect had, it has to
   * report the literal rather than the token. Without this, a reader that
   * matched the wrong rule - or fell back to something constant - would leave
   * the assertion above green over a tint.
   */
  it('reports a row grounded in a literal', () => {
    expect(
      groundIn(`${SELECTOR} { background: rgba(244,118,0,.09); }`, SELECTOR),
      'the tint this row drew before the fix',
    ).toBe('rgba(244,118,0,.09)');
  });
});
