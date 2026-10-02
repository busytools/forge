/**
 * A state class's rule, pinned to the tone that class names.
 *
 * **The defect this closes.** Swapping a token anywhere in the client's own
 * sheet - `details.hooks > summary .st.err` from `var(--bad)` to `var(--ok)`,
 * say - left the whole suite passing, so a failed hook's summary mark would
 * draw in the success tone and nothing would notice. No test pinned a colour
 * token in `assets/web.css` at all, and the same swap was invisible wherever
 * it was made.
 *
 * `Hooks.test.ts` pins one rule of the BOOK DRAWING's copy of the sheet, by
 * regex, selector and token together. That made the drawing's rule better
 * covered than the app's identical one, which is backwards: the app's sheet is
 * what ships. This is the general check rather than a second per-rule pin.
 *
 * **The property rather than a list of rules.** Every rule whose selector
 * carries a state class may only draw in a tone that class names, so a new
 * `.st.err` rule anywhere in the sheet is covered by writing it and nothing
 * here has to be extended to reach it. Pinning the rules a finding happens to
 * name, one at a time, is how a file collects a test per symptom instead.
 *
 * **The token NAMED is the whole claim.** The palette lives in `[web] theme`
 * and reaches the page from the server, so nothing here knows what `--bad`
 * resolves to - only that the rule meant to draw a failure names it.
 * `contrast.test.ts` is the check that sits over the palette's values.
 *
 * Two holes, named rather than left to be discovered. A state-class rule whose
 * colour declaration is DELETED names no tone at all and so leaves this check's
 * scope, where the tone test below reads declarations rather than requiring
 * one. And a state class coloured by a literal is read on `color` only, since
 * the sheet tinting a border or a ground by rgba is its idiom for alpha.
 */
import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

/**
 * The state classes the sheet writes, and the tone each one MEANS.
 *
 * A class belongs here when its NAME fixes its tone - a failure cannot be
 * drawn green - rather than only its rows' convention. `needs` and `unseen`
 * are states whose tone is a design choice, so they are out: the sheet may
 * recolour them without losing anything they are named for.
 */
const TONES = {
  ok: '--ok',
  err: '--bad',
  fail: '--bad',
  failed: '--bad',
  bad: '--bad',
  warn: '--warn',
} as const;

type StateClass = keyof typeof TONES;

/** Every tone the vocabulary can name, which is the palette's state colours. */
const TONE_TOKENS = new Set<string>(Object.values(TONES));

/**
 * A state class carried by a selector.
 *
 * The pattern is built from the vocabulary's own names, so every hit is one of
 * them, and a class added to `TONES` is scannable without a second edit here.
 */
const STATE_CLASS = new RegExp(`\\.(${Object.keys(TONES).join('|')})\\b`, 'g');

function stateClasses(selector: string): StateClass[] {
  // A class inside `:not()` names a state the element is NOT, so it is not a
  // state this selector carries. Nested parentheses would escape this, and
  // none are written.
  const bare = selector.replace(/:not\([^)]*\)/g, '');
  return [...(bare.match(STATE_CLASS) ?? [])].map((hit) => hit.slice(1) as StateClass);
}

/** The palette tones a piece of a sheet names, in the order they appear. */
function tonesNamed(text: string): string[] {
  return [...text.matchAll(/var\((--[\w-]+)\)/g)]
    .map(([, token = '']) => token)
    .filter((token) => TONE_TOKENS.has(token));
}

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

/** A sheet as these checks read it. */
function sheetText(raw: string): string {
  // A brace inside a comment is not a block, and a comment sitting above a
  // rule would otherwise be glued onto the selector the rule parse reads.
  return raw.replace(/\/\*[\s\S]*?\*\//g, '');
}

/** The sheet the app ships. */
const SHEET = sheetText(readFileSync(new URL('./assets/web.css', import.meta.url), 'utf8'));

const PAGE = readFileSync(
  new URL('../../docs/book/src/ui/client/web-session.html', import.meta.url),
  'utf8',
);

/**
 * Every `<style>` block in the drawing, joined rather than the first taken: a
 * second block is where a rule escapes a read that asked for one.
 */
const BOOK = sheetText(
  [...PAGE.matchAll(/<style>([\s\S]*?)<\/style>/g)].map(([, css = '']) => css).join('\n'),
);

const SHEETS: [string, string][] = [
  ['assets/web.css', SHEET],
  ['the book drawing', BOOK],
];

describe('the sheet a state class draws in', () => {
  /**
   * The denominator. An emptiness assertion that has stopped reading anything
   * looks exactly like a clean one, and the tone test below is one: a parse
   * that read a fraction of the sheet reports a short, plausible, confidently
   * clean answer. Every `{` in the sheet is either a rule this parse read or
   * the head of a block it named, and the two counts are taken independently -
   * one from the structured parse, one off the raw text.
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
          'parse has stopped seeing a shape the sheet writes, and read a smaller sheet than ' +
          'exists. Name it in AT_RULE_BLOCK if a nested at-rule was added; if none was, the ' +
          'rule pattern is what moved',
      ).toBe(opens);
    }
  });

  it('draws a state class in the tone that class names', () => {
    for (const [where, sheet] of SHEETS) {
      const wrong: string[] = [];

      for (const { selector, body } of rules(sheet)) {
        const classes = stateClasses(selector);
        if (classes.length === 0) continue;

        const allowed = new Set<string>(classes.map((name) => TONES[name]));
        const names = classes.map((name) => `.${name}`).join(' and ');

        for (const token of tonesNamed(body)) {
          if (!allowed.has(token)) {
            wrong.push(`${where}: "${selector}" draws in ${token}, which ${names} does not name`);
          }
        }

        // The sheet's own contract is that the palette is not here, so a state
        // class drawing text in a literal has left the token set behind.
        for (const [, value = ''] of body.matchAll(/(?:^|;)\s*color\s*:\s*([^;]+)/g)) {
          const [token] = tonesNamed(value);
          if (token === undefined || !allowed.has(token)) {
            wrong.push(
              `${where}: "${selector}" draws its text in ${value.trim()}, which ${names} does ` +
                'not name',
            );
          }
        }
      }

      expect(
        wrong,
        'a state class drawn in a tone it does not name has lost the colour that gives it its ' +
          `meaning: ${wrong.join('; ')}`,
      ).toEqual([]);
    }
  });

  /**
   * Totality over the vocabulary, and the half the tone test cannot reach: a
   * class whose every rule goes silent - renamed, or dropped - leaves nothing
   * to mismatch, so a scan that finds no rules at all reports clean. Each name
   * has to be one the SHIPPING sheet colours in the tone it names.
   */
  it('carries every tone the vocabulary names, in the sheet that ships', () => {
    const coloured = new Set<string>();

    for (const { selector, body } of rules(SHEET)) {
      for (const name of stateClasses(selector)) {
        if (tonesNamed(body).includes(TONES[name])) coloured.add(name);
      }
    }

    expect(
      Object.keys(TONES).filter((name) => !coloured.has(name)),
      'a state class the sheet never draws in its own tone is either one whose tone was dropped ' +
        'or one that was renamed and left this vocabulary behind: name the class the sheet ' +
        'writes now, or put its tone back',
    ).toEqual([]);
  });

  /**
   * The classifier's own spellings, named directly. The tone test asks which
   * classes a selector carries, and a reader that answered short would find
   * fewer rules than exist and report them clean - so the misspellings matter
   * as much as the hits.
   */
  it('names the state classes the sheet writes, and nothing wider', () => {
    expect(stateClasses('.st.err'), 'the mark this defect was found on').toEqual(['err']);
    expect(stateClasses('.kind > summary .st.err'), 'reached through an ancestor').toEqual(['err']);
    expect(stateClasses('.band .dot.ok, .band .dot.warn'), 'a list keeps both').toEqual([
      'ok',
      'warn',
    ]);
    expect(stateClasses('.st.err:hover'), 'a state that is also a target').toEqual(['err']);
    expect(stateClasses('.error'), 'a name that merely starts the same').toEqual([]);
    expect(
      stateClasses('.box:focus-within:not(.rec, .err)'),
      'a state the selector is not',
    ).toEqual([]);
    expect(stateClasses('.row .what a'), 'a selector carrying no state at all').toEqual([]);
  });
});
