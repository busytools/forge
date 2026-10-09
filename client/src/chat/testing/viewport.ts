/**
 * The browser API a mounted column needs and jsdom does not have, with the
 * callback kept.
 *
 * The column watches its scroll content for size changes, because a row laid
 * out after the last pin moves the foot with nothing else to say so. jsdom
 * performs no layout and calls no observer, so the callback is recorded here
 * and `resized()` fires it: a test can then say what a size change does to the
 * reader's place, which is the only way that behaviour is covered at all.
 */
const watching: Array<() => void> = [];

export function installResizeObserver(): void {
  const held = globalThis as { ResizeObserver?: unknown };
  if (held.ResizeObserver !== undefined) return;
  held.ResizeObserver = class {
    constructor(callback: () => void) {
      watching.push(callback);
    }
    observe(): void {
      return undefined;
    }
    unobserve(): void {
      return undefined;
    }
    disconnect(): void {
      return undefined;
    }
  };
}

/** Every observer a mounted column left behind, forgotten between tests. */
export function clearObservers(): void {
  watching.length = 0;
}

/** A size change, as the browser would deliver one. */
export function resized(): void {
  // The entry list is empty: jsdom does no layout, so no row has a size to
  // report - and an observer that reads its entries must not be handed
  // nothing at all.
  for (const callback of watching) {
    (callback as (entries: ResizeObserverEntry[]) => void)([]);
  }
}
