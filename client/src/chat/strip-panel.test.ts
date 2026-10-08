// @vitest-environment jsdom
import { describe, expect, it } from 'vitest';

import { panelStyle } from './strip-panel';

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
});
