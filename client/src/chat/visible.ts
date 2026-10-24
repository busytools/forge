/**
 * Run `paint` once, the first time `node` is on screen.
 *
 * **The colour waits for the viewport, not for the mount.** A heavy turn is
 * one row to the list, so every one of its code panels is in the DOM at once -
 * and colouring a diff can hold the main thread for most of a second, so a
 * panel nobody can see is drawn plain and tokenised only when it scrolls into
 * view. The plain pass already carries every word, which is the panel's own
 * existing rule.
 *
 * **One observer for the page, not one per row.** The same heavy turn mounts
 * dozens of panels; sharing the observer keeps the browser's own bookkeeping
 * to one object and is the seam a later lazy surface can use too.
 *
 * **A page without an observer paints at once.** A server render and the test
 * environment have no layout to be visible in.
 */
let observer: IntersectionObserver | null = null;
const waiting = new WeakMap<Element, () => void>();

function shared(): IntersectionObserver {
  if (observer === null) {
    observer = new IntersectionObserver((entries) => {
      for (const entry of entries) {
        if (!entry.isIntersecting) continue;
        const paint = waiting.get(entry.target);
        if (paint === undefined) continue;
        waiting.delete(entry.target);
        observer?.unobserve(entry.target);
        paint();
      }
    });
  }
  return observer;
}

/**
 * `node`'s first entry into the viewport runs `paint`.
 *
 * The answer unregisters the ask, so a component that unmounts or re-renders
 * before it is seen does not leave a dead entry behind.
 */
export function whenVisible(node: Element, paint: () => void): () => void {
  if (typeof IntersectionObserver === 'undefined') {
    paint();
    return () => undefined;
  }
  const seen = shared();
  waiting.set(node, paint);
  seen.observe(node);
  return () => {
    if (!waiting.delete(node)) return;
    seen.unobserve(node);
  };
}
