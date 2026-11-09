/**
 * The rows that hand a take card the line.
 *
 * **The defect behind this file.** A take card (`.tc`) is a pill of its own
 * content's width, so a row that hosts one has to say `flex: 1 1 auto` and
 * `min-width: 0` or the card collapses to its 7px dot: the clock, the wave
 * and the counts all shrink to zero, and the card's own `x` overflows into
 * whatever control stands beside it. The composer's line, the dock's words
 * row and the takeover bar's feedback line were each given that rule; the
 * held card's send-back box was not, so a reader who dictated their feedback
 * saw a dot in a ring and the take's `x` sitting on the Send back button -
 * the state Ved photographed on 2026-11-09 over a held Slack post.
 *
 * **Why a rule-level pin rather than a rendered one.** The take card's width
 * is a layout fact and jsdom performs no layout, so a mounted card would be
 * 0px wide in every case, fixed or not. What is checkable here is the sheet's
 * own claim - and it is the half that regressed: the rule existed, and the
 * send-back box's selector was simply not in its list. A fifth row that hosts
 * a take card belongs in the same three lists, and this file fails until it
 * is.
 */

import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

/** Every `selector { body }` in the sheet, comments stripped, whitespace folded. */
function rulesIn(source: string): { selector: string; body: string }[] {
  const code = source.replace(/\/\*[\s\S]*?\*\//g, '');
  const found: { selector: string; body: string }[] = [];
  let start = 0;
  for (let at = 0; at < code.length; at += 1) {
    const ch = code[at];
    if (ch === '{') {
      const head = code.slice(start, at).trim().replace(/\s+/g, ' ');
      if (head.startsWith('@')) {
        start = at + 1;
        continue;
      }
      const close = code.indexOf('}', at);
      if (close === -1) break;
      found.push({ selector: head, body: code.slice(at + 1, close).replace(/\s+/g, ' ') });
      at = close;
      start = close + 1;
    } else if (ch === '}') {
      start = at + 1;
    }
  }
  return found;
}

/** The selector lists of every rule in the sheet, split on commas. */
function selectorsIn(source: string): { selector: string; body: string }[] {
  return rulesIn(source).flatMap((rule) =>
    rule.selector.split(',').map((selector) => ({ selector: selector.trim(), body: rule.body })),
  );
}

const SHEET = readFileSync(new URL('./web.css', import.meta.url), 'utf8');
const RULES = selectorsIn(SHEET);

/** Every row that draws a spread take card, and the words a reader knows it by. */
const ROWS = [
  ['.takeline', "the composer's own line"],
  ['.dock .qrow', "the dock's words row"],
  ['.tb-feedback', "the takeover bar's feedback line"],
  ['.dock .sendback', "a held card's send-back box"],
] as const;

/** The body of the one rule that names `selector` and sets `property`, or null. */
function ruleSetting(selector: string, property: string): string | null {
  return (
    RULES.find((rule) => rule.selector === selector && rule.body.includes(property))?.body ?? null
  );
}

describe('the rows that hand a take card the line', () => {
  it('gives the card the row and lets its readings shrink rather than collapse', () => {
    for (const [row, name] of ROWS) {
      const card = ruleSetting(`${row} .tc`, 'flex: 1 1 auto');
      expect(card, `${name} hands its take card no width`).not.toBeNull();
      expect(card, `${name} lets its take card overflow the row`).toContain('min-width: 0');
    }
  });

  it('stretches the transcribing bar and its ticks to the room the card got', () => {
    for (const [row, name] of ROWS) {
      expect(
        ruleSetting(`${row} .tc .bar`, 'flex: 1 1 auto'),
        `${name} leaves the transcribing bar at the pill's width`,
      ).not.toBeNull();
      expect(
        ruleSetting(`${row} .tc .ticks`, 'flex: 1 1 auto'),
        `${name} leaves the transcribing ticks at the pill's width`,
      ).not.toBeNull();
    }
  });
});
