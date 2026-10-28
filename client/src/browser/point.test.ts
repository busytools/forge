import { describe, expect, it } from 'vitest';

import { pagePoint } from './point';

/** A box as the picture is drawn, and the page area it shows. */
const box = { left: 100, top: 50, width: 720, height: 450 };
const page = { width: 720, height: 450 };

describe('a tap on the picture', () => {
  /**
   * **A letterboxed picture is not one canvas pixel to one page pixel.**
   * The phone's portrait area is smaller than the wide page's frame, so the
   * canvas is drawn scaled to fit; reading the client point straight put
   * every tap at the wrong place on the page (measured on the dev stack: a
   * text field could be clicked but never focused).
   */
  it('scales through a letterboxed box to the page its frame shows', () => {
    const picture = { left: 10, top: 20, width: 360, height: 225 };
    expect(pagePoint(190, 132.5, picture, page), 'the centre stays the centre').toEqual({
      x: 360,
      y: 225,
    });
    expect(pagePoint(10, 20, picture, page), 'the top-left is the page origin').toEqual({
      x: 0,
      y: 0,
    });
    expect(pagePoint(370, 245, picture, page), 'and the bottom-right is the page corner').toEqual({
      x: 720,
      y: 450,
    });
  });

  it('is the point itself when the picture already fills the page', () => {
    expect(pagePoint(460, 275, box, page)).toEqual({ x: 360, y: 225 });
  });

  /** A box with no extent is a layout before its first measure, not a division by zero. */
  it('answers the page origin for a box that is not laid out yet', () => {
    const empty = { left: 0, top: 0, width: 0, height: 0 };
    expect(pagePoint(5, 5, empty, page)).toEqual({ x: 0, y: 0 });
  });

  /** A frame that names no page has no point to send a gesture to. */
  it('answers the page origin for a frame that names no page', () => {
    expect(pagePoint(5, 5, box, { width: 0, height: 0 })).toEqual({ x: 0, y: 0 });
  });
});
