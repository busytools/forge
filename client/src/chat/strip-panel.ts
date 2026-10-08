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
 * **The limits are the ROOM, and there is no floor.** The panel is anchored
 * to its segment's right edge and grows leftward, so a segment sitting
 * part-way off the viewport - measured under a folded rail, the strip itself
 * wrapping rather than scrolling, so this arm is the guard-rail for whatever
 * puts one there (Android keyboard and IME behaviour is unmeasured) - has its
 * anchor pulled back in first, and what remains (the width left of it, the
 * height above it) is what the panel may take. A floor is what drew panels
 * 80px off a phone's left edge and clipped their tops (Ved, 2026-10-08):
 * below the floor the panel fits the room and stays reachable.
 *
 * The width also stops at a readable measure, so a long command ellipsizes
 * in its row instead of growing the panel: the detail stays a panel, not a
 * page. It is emitted as the panel's WIDTH rather than a ceiling, so the
 * panel keeps one size while a reveal under a row comes and goes.
 */
export function panelStyle(el: HTMLElement | null): string {
  if (el === null) return '';
  const box = el.getBoundingClientRect();
  // No layout (or not yet drawn): the sheet's own caps stand.
  if (box.right <= 0 || box.top <= 0) return '';
  // The edge the panel hangs from, inside the viewport whatever the strip's
  // own scroll did to the segment.
  const right = Math.min(box.right, window.innerWidth - 6);
  const inset = Math.round(box.right - right);
  // -18, not -12: the panel's bottom sits 6px above the segment, so this
  // lands its top 12px under the viewport's. The zero floors are honesty
  // rather than shape: a room past its own edge would emit a negative
  // declaration, which the parser drops and the sheet's own caps would take
  // back.
  const height = Math.round(Math.max(0, Math.min(window.innerHeight * 0.6, box.top - 18)));
  const width = Math.round(Math.max(0, Math.min(780, right - 12)));
  return `right:${inset}px;width:${width}px;max-height:${height}px`;
}
