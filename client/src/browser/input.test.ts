import { describe, expect, it } from 'vitest';

import { modifiers, toPage } from './input';

describe('the input mapping', () => {
  const stage = { left: 40, top: 84, width: 400, height: 300 };

  it('maps a client point into the page, scaled and offset', () => {
    // The canvas shows a 800x600 page in a 400x300 box at (40, 84): half
    // scale, both axes.
    expect(toPage({ stage, page: { width: 800, height: 600 } }, { x: 40, y: 84 })).toEqual({
      x: 0,
      y: 0,
    });
    expect(toPage({ stage, page: { width: 800, height: 600 } }, { x: 240, y: 234 })).toEqual({
      x: 400,
      y: 300,
    });
    expect(toPage({ stage, page: { width: 800, height: 600 } }, { x: 440, y: 384 })).toEqual({
      x: 800,
      y: 600,
    });
  });

  it('answers null rather than a guess when there is nothing to map onto', () => {
    expect(toPage({ stage, page: { width: 0, height: 0 } }, { x: 100, y: 100 })).toBeNull();
    expect(
      toPage(
        { stage: { ...stage, width: 0 }, page: { width: 800, height: 600 } },
        { x: 100, y: 100 },
      ),
    ).toBeNull();
  });

  it("builds CDP's own modifier mask", () => {
    expect(modifiers({ altKey: false, ctrlKey: false, metaKey: false, shiftKey: false })).toBe(0);
    expect(modifiers({ altKey: true, ctrlKey: false, metaKey: false, shiftKey: false })).toBe(1);
    expect(modifiers({ altKey: false, ctrlKey: true, metaKey: false, shiftKey: false })).toBe(2);
    expect(modifiers({ altKey: false, ctrlKey: false, metaKey: true, shiftKey: false })).toBe(4);
    expect(modifiers({ altKey: true, ctrlKey: true, metaKey: true, shiftKey: true })).toBe(15);
  });
});
