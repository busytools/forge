/**
 * Where a pointer event lands in the PAGE's own pixels.
 *
 * The canvas carries the page at 1:1 in its backing store and CSS scales its
 * box down to fit the stage, so the canvas's own bounding box IS the drawn
 * page: the mapping is the canvas rect against the page's size. Pure, so the
 * geometry is testable apart from the canvas the frames are drawn on.
 */

/** The canvas's box and the page it shows, all in CSS pixels. */
export interface Geometry {
  stage: { left: number; top: number; width: number; height: number };
  page: { width: number; height: number };
}

/**
 * A client point in the page's pixels, or `null` when there is nothing to map
 * onto yet (no frame, or a canvas that has not been laid out).
 */
export function toPage(
  geometry: Geometry,
  client: { x: number; y: number },
): { x: number; y: number } | null {
  const { stage, page } = geometry;
  if (stage.width <= 0 || stage.height <= 0 || page.width <= 0 || page.height <= 0) {
    return null;
  }
  return {
    x: Math.round(((client.x - stage.left) / stage.width) * page.width),
    y: Math.round(((client.y - stage.top) / stage.height) * page.height),
  };
}

/** The CDP modifier bitmask for a keyboard event: alt 1, ctrl 2, meta 4, shift 8. */
export function modifiers(event: {
  altKey: boolean;
  ctrlKey: boolean;
  metaKey: boolean;
  shiftKey: boolean;
}): number {
  return (
    (event.altKey ? 1 : 0) |
    (event.ctrlKey ? 2 : 0) |
    (event.metaKey ? 4 : 0) |
    (event.shiftKey ? 8 : 0)
  );
}
