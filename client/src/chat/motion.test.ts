// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';

import { arrival, drawnAt, land, lastDrawn, noteArrival, rise } from './motion';

/** Whether a media query matches, as the page's preference reads it. */
function withMotion(on: boolean): void {
  vi.stubGlobal('matchMedia', (query: string) => ({
    matches: !on && query.includes('reduced-motion'),
    media: query,
    onchange: null,
    addListener: () => {},
    removeListener: () => {},
    addEventListener: () => {},
    removeEventListener: () => {},
    dispatchEvent: () => false,
  }));
}

/** An element whose geometry jsdom does not compute, stated by the test. */
function measured(rect: { left: number; top: number; width: number; height: number }): HTMLElement {
  const node = document.createElement('div');
  node.getBoundingClientRect = () => ({
    ...rect,
    x: rect.left,
    y: rect.top,
    right: rect.left + rect.width,
    bottom: rect.top + rect.height,
    toJSON: () => ({}),
  });
  document.body.append(node);
  return node;
}

afterEach(() => {
  document.body.innerHTML = '';
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe('the one motion a message makes', () => {
  it("hands a card's last position to exactly one landing", () => {
    drawnAt('p-1', { left: 10, top: 20, width: 100, height: 40 });
    expect(lastDrawn('p-1'), 'the card was drawn here').toEqual({
      left: 10,
      top: 20,
      width: 100,
      height: 40,
    });
    expect(lastDrawn('p-1'), 'and the row it became claimed it').toBeNull();
    expect(lastDrawn('p-never'), 'a row no card was drawn for just appears').toBeNull();
  });

  it('lands the row from the card position, and settles its own style', () => {
    vi.useFakeTimers();
    withMotion(true);
    const row = measured({ left: 30, top: 50, width: 50, height: 20 });
    land(row, { left: 10, top: 20, width: 100, height: 40 });

    // The one item moves: from where the card was to where the row is.
    expect(row.style.transform).toBe('translate(0, 0) scale(1, 1)');
    expect(row.style.transformOrigin).toBe('top left');
    expect(row.style.opacity).toBe('1');

    vi.advanceTimersByTime(400);
    expect(row.style.transform, 'the travelling style is not left behind').toBe('');
    expect(row.style.transition).toBe('');
    expect(row.style.opacity).toBe('');
  });

  it('keeps the position change and drops the animation under reduced motion', () => {
    withMotion(false);
    const row = measured({ left: 30, top: 50, width: 50, height: 20 });
    land(row, { left: 10, top: 20, width: 100, height: 40 });
    expect(row.style.transform, 'the row arrives where it belongs').toBe('');
    expect(row.style.transition).toBe('');
  });

  it('rises a row that arrived with no card to move from, once', () => {
    vi.useFakeTimers();
    withMotion(true);
    noteArrival('p-2');
    expect(arrival('p-2'), 'the row appended live may rise').toBe(true);
    expect(arrival('p-2'), 'and only its first mount').toBe(false);

    const row = measured({ left: 0, top: 0, width: 40, height: 20 });
    rise(row);
    expect(row.style.transform).toBe('translateY(0)');
    vi.advanceTimersByTime(400);
    expect(row.style.transform).toBe('');
  });
});
