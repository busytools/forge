/**
 * The space a strip panel may take, as a style string, measured where its
 * segment sits.
 *
 * **The limits cannot be pure CSS.** The panel hangs above its segment
 * (`bottom: calc(100% + 6px)`), so what bounds it is the distance from the
 * segment to the viewport's top and left edges - a fact about the strip's
 * place on the page, which no viewport-relative rule can see. Measured
 * 2026-10-06 on a wide window: a viewport cap alone let a row's 200-character
 * command stretch the panel past the left edge (headlines clipped mid-word)
 * and past the top one.
 *
 * The width also stops at a readable measure, so a long command ellipsizes
 * in its row instead of growing the panel: the detail stays a panel, not a
 * page.
 */
export function panelStyle(el: HTMLElement | null): string {
  if (el === null) return '';
  const box = el.getBoundingClientRect();
  // No layout (or not yet drawn): the sheet's own caps stand.
  if (box.right <= 0 || box.top <= 0) return '';
  // -18, not -12: the panel's bottom sits 6px above the segment, so this
  // lands its top 12px under the viewport's.
  const height = Math.round(Math.max(180, Math.min(window.innerHeight * 0.6, box.top - 18)));
  const width = Math.round(Math.max(330, Math.min(680, box.right - 12)));
  return `max-height:${height}px;max-width:${width}px`;
}
