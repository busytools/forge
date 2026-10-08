// @vitest-environment jsdom
import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

import { panelStyle } from './strip-panel';

// Relative to the client root, which is where vitest runs: under jsdom the
// module's own URL is not a file one.
const SHEET = readFileSync('src/assets/web.css', 'utf8');

/**
 * Every rule block for one selector, from any depth.
 *
 * **All of them, media arms included**: the compact arm's floor was invisible
 * to a matcher that read one top-level block, and it beat the measured width
 * on every viewport under 560px. The selector has to be the whole line's
 * selector (indent allowed), so a scrollbar arm carrying the same word does
 * not answer for the rule.
 */
const rules = (selector: string): string[] => {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const pattern = new RegExp(`(?:^|\\n)[\\t ]*${escaped} \\{([^}]*)\\}`, 'g');
  const found = [...SHEET.matchAll(pattern)].map((match) => match[1] ?? '');
  if (found.length === 0) throw new Error(`no ${selector} rule in the sheet`);
  return found;
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
    // sheet, EVERY arm of it: the compact one under 560px beat the measured
    // width where a single top-level match could not see it.
    for (const selector of ['.sg-list', '.bz-list']) {
      const blocks = rules(selector);
      expect(blocks.length, `${selector} rules found`).toBeGreaterThan(0);
      for (const block of blocks) {
        expect(block, `${selector} carries no min-width floor`).not.toContain('min-width');
      }
      expect(blocks.join('\n'), `${selector} keeps a viewport cap somewhere`).toContain(
        'max-width: calc(100vw',
      );
    }
  });
});
