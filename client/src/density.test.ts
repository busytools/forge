import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

/**
 * The conversation's density, pinned to numbers.
 *
 * **Read off the sheet, not off the DOM.** Every property here is spacing -
 * leading, a margin, a padding - and a markup assertion cannot see any of
 * them; a rule that is present but never matches an element is invisible to a
 * layout assertion too. So these read the declarations, the way
 * `contrast.test.ts` reads the tokens and `Session.test.ts` reads a rule body.
 *
 * **The numbers are the terminal's.** `scripts/density/measure.mjs` measures
 * the same fixture through the TUI's own render path and through a real
 * browser, and these are the client side of that comparison brought to the
 * terminal's: one blank row's worth between blocks, a row that is a line plus
 * a hairline rather than a line plus a third, and a leading nearer a terminal
 * cell than a book page. Each assertion names the number it is protecting so a
 * change to one is a deliberate edit here rather than a silent drift.
 */

const SHEET = readFileSync(new URL('./assets/web.css', import.meta.url), 'utf8').replace(
  /\s*\{/g,
  ' {',
);

/**
 * The declarations of the first rule whose selector contains `fragment`.
 *
 * A fragment rather than a whole selector, because the heading rule is a list
 * six selectors long and the table rule is a pair. Matching a fragment keeps
 * the test about the declaration rather than about the spelling of the list.
 */
function body(fragment: string): string {
  // Escaped: `.prose p + p` is a selector, and an unescaped `+` is a
  // quantifier that then matches nothing and reports the rule as missing.
  const escaped = fragment.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const pattern = new RegExp(`(^|\\})([^{}]*${escaped}[^{}]*)\\{([^{}]*)\\}`, 'm');
  const found = pattern.exec(SHEET)?.[3];
  expect(found, `the sheet has no rule for ${fragment}`).toBeDefined();
  return found ?? '';
}

/**
 * One property, as the sheet resolves it for a selector containing `fragment`.
 *
 * The LAST rule declaring it, not the first: equal specificity means the later
 * declaration is the one that draws, and `.prose th` is written across two
 * rules - the pair it shares with `.prose td`, and the header's own.
 */
function declared(fragment: string, property: string): string {
  const escaped = fragment.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const rules = new RegExp(`(^|\\})([^{}]*${escaped}[^{}]*)\\{([^{}]*)\\}`, 'gm');
  const found = [...SHEET.matchAll(rules)]
    .flatMap((hit) => {
      const value = new RegExp(`(?:^|;)\\s*${property}:\\s*([^;]+)`).exec(hit[3] ?? '')?.[1];
      return value === undefined ? [] : [value];
    })
    .at(-1);
  expect(found, `${fragment} declares no ${property}`).toBeDefined();
  return found?.trim() ?? '';
}

/** Every weight the sheet's own `@font-face` blocks declare, ascending. */
function shippedCuts(): number[] {
  return [...SHEET.matchAll(/@font-face[^{]*\{([^{}]*)\}/g)]
    .flatMap(([, rule]) => {
      const weight = /font-weight:\s*(\d+)/.exec(rule ?? '')?.[1];
      return weight === undefined ? [] : [Number(weight)];
    })
    .sort((one, two) => one - two);
}

describe("the conversation draws at the terminal's density", () => {
  /**
   * **Nothing in the prose is synthesized.** Fira Code ships static cuts, so a
   * weight above the heaviest one declared has no face to draw with and the
   * browser fakes it by smearing the Medium cut - which is what made three
   * emphasised phrases read as three loud things. `strong` carries no rule of
   * its own before this, so it drew the browser's default 700.
   */
  it('asks only for weights the shipped face has a cut for', () => {
    const cuts = shippedCuts();
    // A control: with no faces read, every assertion below would hold
    // vacuously and the test would be green for the wrong reason.
    expect(cuts, 'the sheet declares no @font-face weight').not.toHaveLength(0);
    const heaviest = Math.max(...cuts);

    for (const mark of ['.prose strong', '.prose th', '.prose h1']) {
      expect(Number(declared(mark, 'font-weight')), `${mark} asks for a synthesized weight`).toBe(
        heaviest,
      );
    }
  });

  /**
   * The leading, in the unit the comparison was taken in: the terminal draws
   * one row per line, which is 1.231em of its text size. 1.5em is a book page;
   * 1.35 is nearer a terminal's cell while staying off it, since a browser
   * still has to fit a descender under a line of monospace.
   */
  it('leads the conversation at 1.35', () => {
    expect(declared('.conv', 'line-height')).toBe('1.35');
    expect(declared('.code pre', 'line-height')).toBe('1.35');
  });

  /**
   * A code panel is a line plus a hairline of padding. It was a line plus a
   * third - 8px of padding top and bottom, which is 0.94em of air around one
   * line of 14px code, and 1.89x what the terminal draws for the same fence.
   */
  it('pads a code panel to less than a line', () => {
    expect(declared('.code pre', 'padding')).toBe('6px 10px');
    expect(declared('.code .lang', 'padding')).toBe('4px 10px');
  });

  /**
   * A table row is a line, near enough. 4px top and bottom took a row to
   * 1.96em against the terminal's 1.25em, which is the airiness that made a
   * six-row state table read as a page of its own.
   */
  it('pads a table row to a hairline', () => {
    expect(declared('.prose th', 'padding')).toBe('2px 12px 2px 0');
  });

  /**
   * **One blank line between blocks**, which is the terminal's entire spacing
   * policy: a streamed reply is cut at paragraph boundaries and each block
   * carries one trailing break. Ten pixels was 0.59em against the terminal's
   * 1.23em, and this is the half of the change that makes the message taller.
   */
  it('separates two blocks by one line', () => {
    // Either spelling: a rule between siblings only owes a top, and one that
    // draws on both sides owes the pair.
    const gap = (selector: string) => {
      const found = /margin(?:-top)?:\s*([^;]+)/.exec(body(selector))?.[1];
      expect(found, `${selector} declares no margin`).toBeDefined();
      return found?.trim() ?? '';
    };

    for (const block of [
      '.prose p + p',
      '.prose ul',
      '.prose table',
      '.prose .code',
      '.prose blockquote',
      '.prose h1',
    ]) {
      expect(gap(block), `${block} does not carry the block gap`).toContain('20px');
    }
  });
});
