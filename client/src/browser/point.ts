/**
 * A touch on the picture, as the page's own CSS pixel.
 *
 * **The canvas is scaled to fit, so one canvas pixel is not always one page
 * pixel**: a wide frame in a portrait area is letterboxed, and its element
 * is smaller than the page it shows. The element's box is the picture on
 * screen; the page area is the viewport the page was laid out at. Mapping
 * between them keeps a tap on a field on the field.
 */

/** A client point, as `getBoundingClientRect` reports one. */
export interface Box {
  left: number;
  top: number;
  width: number;
  height: number;
}

/** The page area's size, in the page's own CSS pixels. */
export interface Page {
  width: number;
  height: number;
}

export function pagePoint(
  clientX: number,
  clientY: number,
  box: Box,
  page: Page,
): { x: number; y: number } {
  // A picture whose box has no extent is a layout before its first measure;
  // the page's origin is the one point that means nothing rather than garbage.
  if (box.width <= 0 || box.height <= 0) return { x: 0, y: 0 };
  return {
    x: ((clientX - box.left) * page.width) / box.width,
    y: ((clientY - box.top) * page.height) / box.height,
  };
}
