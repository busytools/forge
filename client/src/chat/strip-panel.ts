/**
 * The space a strip panel may take, as a style string, measured where its
 * segment sits.
 *
 * **The limits cannot be pure CSS.** The panel hangs above its segment, so
 * what bounds it is the distance from the segment to the viewport's top and
 * left edges - a fact about the strip's place on the page, which no
 * viewport-relative rule can see. Measured 2026-10-06 on a wide window: a
 * viewport cap alone let a row's 200-character command stretch the panel past
 * the left edge (headlines clipped mid-word) and past the top one.
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
  // **Viewport terms, not segment ones.** The panel is portaled to the body
  // (see `portal`), so `right:` resolves against the initial containing
  // block, which coincides with the viewport only because the session page's
  // document does not scroll: an inset kept in segment terms drew the panel's
  // right edge at the viewport's own - measured, 23px past a 1577px segment
  // on a 1600px window, and flush with the screen edge on a phone.
  const inset = Math.round(Math.max(0, window.innerWidth - right));
  // -18, not -12: the panel's bottom sits 6px above the segment, so this
  // lands its top 12px under the viewport's. The zero floors are honesty
  // rather than shape: a room past its own edge would emit a negative
  // declaration, which the parser drops and the sheet's own caps would take
  // back.
  const height = Math.round(Math.max(0, Math.min(window.innerHeight * 0.6, box.top - 18)));
  const width = Math.round(Math.max(0, Math.min(780, right - 12)));
  // The panel is portaled to the body (see `portal`), so its anchor above the
  // segment has to be stated in viewport terms too - `bottom: calc(100% + 6px)`
  // measured the segment only while the panel was its child.
  const bottom = Math.round(Math.max(0, window.innerHeight - box.top + 6));
  return `right:${inset}px;bottom:${bottom}px;width:${width}px;max-height:${height}px`;
}

/**
 * Each portaled panel's anchor: the element it hangs off, recorded when it
 * enters the body.
 *
 * **A portaled panel is nobody's DOM child**, so a listener asking whether a
 * pointer landed inside someone's panel cannot see it by containment. The
 * anchor is what says which segment drew it, and so which panel it belongs
 * to for a dismissal that walks the tree.
 */
const anchors = new WeakMap<HTMLElement, HTMLElement | null>();

/**
 * Move a node to the document body for the length of its life.
 *
 * **The strip's chips lane scrolls, so a panel drawn inside it is cut to
 * nothing** - measured: the panel kept its rect and painted a 2px sliver. A
 * panel is positioned in viewport coordinates (`panelStyle` measures rects),
 * which is what the body can carry; Svelte has no portal of its own.
 *
 * `anchor` is the segment the panel hangs off, which `portaledFrom` reads. A
 * caller with no containing panel leaves it out: an unpaired panel is never
 * inside anything.
 */
export function portal(
  node: HTMLElement,
  anchor: HTMLElement | null = null,
): { destroy: () => void } {
  document.body.appendChild(node);
  anchors.set(node, anchor);
  return {
    destroy() {
      node.remove();
    },
  };
}

/**
 * Whether `target` sits inside a panel whose anchor is inside `root`.
 *
 * **This is what keeps a nested panel alive.** A chip behind the `+N` panel
 * opens its own panel, which is portaled to the body and so not a descendant
 * of the `+N` panel. A dismisser that only asks "is this node inside my
 * panel" reads that pointer as away and tears the chip's panel - and the link
 * being clicked - down before the click lands.
 *
 * The nearest portaled ancestor decides: a panel opened from a chip inside
 * `root` belongs to it, and one opened from the strip above the composer does
 * not.
 */
export function portaledFrom(target: Node, root: HTMLElement | null): boolean {
  if (root === null) return false;
  let at: Node | null = target;
  while (at !== null) {
    if (at instanceof HTMLElement && anchors.has(at)) {
      const anchor = anchors.get(at) ?? null;
      return anchor !== null && root.contains(anchor);
    }
    at = at.parentNode;
  }
  return false;
}
