/**
 * The browser API a mounted column needs and jsdom does not have, with the
 * callback kept - and the node it observes.
 *
 * The column watches its scroll content for size changes, because a row laid
 * out after the last pin moves the foot with nothing else to say so. jsdom
 * performs no layout and calls no observer, so the callback is recorded here
 * and `resized()` fires it: a test can then say what a size change does to the
 * reader's place, which is the only way that behaviour is covered at all. The
 * NODE is kept with it because a remembered row height needs a number: with a
 * height handed in, `resized` delivers a synthetic entry per LIVE observed
 * node, which is how a row's measurement gets its value in jsdom.
 */
const watching: Array<{ callback: ResizeObserverCallback; node: Element | null }> = [];

export function installResizeObserver(): void {
  const held = globalThis as { ResizeObserver?: unknown };
  if (held.ResizeObserver !== undefined) return;
  held.ResizeObserver = class {
    callback: ResizeObserverCallback;
    /**
     * The instance's ONE record, pushed at construction.
     *
     * **One per instance, not per `observe`**: an observer watching two nodes
     * - the column's own watcher watches its viewport and its content -
     * registers once, and a callback fired once per observed node re-ran
     * every pass twice, which the pin counts read.
     */
    record: { callback: ResizeObserverCallback; node: Element | null };
    constructor(callback: ResizeObserverCallback) {
      this.callback = callback;
      this.record = { callback, node: null };
      watching.push(this.record);
    }
    observe(node: Element): void {
      this.record.node = node;
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

/**
 * A size change, as the browser would deliver one.
 *
 * Without a height the entry list is empty - jsdom does no layout, so no row
 * has a size to report - and an observer that reads its entries must not be
 * handed nothing at all. With one, every observer of a node still IN the
 * document gets a synthetic `contentRect` of that height: a tree an earlier
 * test unmounted must not take a measurement it can no longer hold.
 */
export function resized(height?: number, skip?: (node: Element) => boolean): void {
  for (const { callback, node } of watching) {
    if (height !== undefined && (node === null || !node.isConnected)) continue;
    // A node the test holds back reports nothing, which is how a row a browser
    // never measured - a held one that draws nothing - gets its zero.
    if (node !== null && skip?.(node) === true) continue;
    const entries =
      height === undefined
        ? []
        : [{ target: node, contentRect: { height } } as ResizeObserverEntry];
    callback(entries, undefined as unknown as ResizeObserver);
  }
}
