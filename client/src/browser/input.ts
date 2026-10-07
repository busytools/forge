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

/**
 * The CDP code for a key that ACTS rather than types. Key and code alone
 * deliver a text key (its own `text` carries it) but reach the page as a
 * no-op for these - Enter without its code never submits a form.
 */
const ACTING_KEYS: Record<string, number> = {
  Enter: 13,
  Backspace: 8,
  Tab: 9,
  Delete: 46,
  ArrowLeft: 37,
  ArrowUp: 38,
  ArrowRight: 39,
  ArrowDown: 40,
  Home: 36,
  End: 35,
  PageUp: 33,
  PageDown: 34,
};

/**
 * What CDP needs beyond `key` and `code` for one keystroke: the Windows
 * virtual key code every key carries, and the `text` only a typing key
 * generates - a character types itself, and Enter types a carriage return,
 * which is what a form reads as submit.
 */
export function keyStroke(key: string): { windowsVirtualKeyCode: number; text?: string } {
  const acting = ACTING_KEYS[key];
  if (acting !== undefined) {
    return key === 'Enter'
      ? { windowsVirtualKeyCode: acting, text: '\r' }
      : { windowsVirtualKeyCode: acting };
  }
  if (key.length === 1) {
    return { windowsVirtualKeyCode: key.toUpperCase().charCodeAt(0), text: key };
  }
  // A key this map has never met goes as itself: CDP takes it, and inventing
  // a code for it would be a guess the page would act on.
  return { windowsVirtualKeyCode: 0 };
}
