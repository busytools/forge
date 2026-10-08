// @vitest-environment jsdom
import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

import { panelStyle } from './strip-panel';

// Relative to the client root, which is where vitest runs: under jsdom the
// module's own URL is not a file one.
const SHEET = readFileSync('src/assets/web.css', 'utf8');

/** One rule's block in the sheet, from its own line to the first close. */
const rule = (selector: string): string => {
  // The selector at the start of a line, so a scrollbar arm's selector list
  // carrying the same word does not answer for the rule.
  const at = SHEET.indexOf(`\n${selector} {`);
  if (at === -1) throw new Error(`no ${selector} rule in the sheet`);
  return SHEET.slice(at, SHEET.indexOf('}', at));
};

/**
 * The room a strip panel may take, measured against the segment's own box.
 *
 * A segment 250px in on a 390px phone has 238px of room to its left, and a
 * panel floored to 330 drew 80px off the screen's left edge (Ved,
 * 2026-10-08) - so the limits are the ROOM, never a floor.
 */
const sheet = (width: number): void => {
  Object.defineProperty(window, 'innerWidth', { value: width, configurable: true });
};

/** An element whose box the test chooses, which is all the function reads. */
const at = (right: number, top: number): HTMLElement =>
  ({ getBoundingClientRect: () => ({ right, top }) }) as unknown as HTMLElement;

const number = (style: string, key: string): number => {
  const match = new RegExp(`${key}:(-?\\d+)`).exec(style);
  if (match === null) throw new Error(`no ${key} in ${JSON.stringify(style)}`);
  return Number(match[1]);
};

describe('the room a strip panel may take', () => {
  it('fits the room left of the segment, with no floor to overflow it', () => {
    sheet(390);
    const style = panelStyle(at(250, 300));
    expect(number(style, 'width'), 'the panel fits the room').toBeLessThanOrEqual(238);
  });

  it('keeps its top inside the room above the segment, with no floor', () => {
    sheet(390);
    const style = panelStyle(at(380, 100));
    expect(number(style, 'max-height'), 'the room above, not a floored 180').toBeLessThanOrEqual(
      82,
    );
  });

  it('pulls a segment part-way off the right back into the viewport', () => {
    sheet(390);
    const style = panelStyle(at(400, 300));
    const inset = number(style, 'right');
    const width = number(style, 'width');
    expect(400 - inset, 'the right edge stays on screen').toBeLessThanOrEqual(390);
    expect(400 - inset - width, 'and so does the left').toBeGreaterThanOrEqual(12);
  });

  it('hands a segment whose room is past its own edge a zero, not a negative', () => {
    // A negative declaration is dropped by the parser and the sheet's own
    // caps come back: zero is the honest floor.
    sheet(390);
    const corner = panelStyle(at(6, 6));
    expect(number(corner, 'width'), 'no negative width').toBe(0);
    expect(number(corner, 'max-height'), 'no negative height').toBe(0);
  });

  it('keeps the sheet free of floors, which no engine test can see', () => {
    // jsdom performs no layout, so a restored `min-width` in the sheet passes
    // every mount test and then reproduces the exact reported symptom in a
    // real engine - the floor lives in the sheet, so it is pinned in the
    // sheet.
    for (const selector of ['.sg-list', '.bz-list']) {
      expect(rule(selector), `${selector} carries no min-width floor`).not.toContain('min-width');
      expect(rule(selector), `${selector} keeps a viewport cap`).toContain('max-width: calc(100vw');
    }
  });
});
