/**
 * The Svelte roots a hand-built row owns, held by the element each was
 * mounted into.
 *
 * The canvas builds its rows as DOM, so nothing Svelte-side tears a root down
 * when a row leaves: a row can go without the reconciler dropping it - a
 * folded file, a scope or a mode switch, the canvas going away - so the held
 * roots are swept for elements that have left the document rather than only
 * released where a row is dropped. A Map rather than a WeakMap: the sweep has
 * to walk it.
 */
export class Mounts {
  readonly #held = new Map<Element, () => void>();

  /** Take a root's disposer, under the element it was mounted into. */
  hold(host: Element, letGo: () => void): void {
    this.#held.set(host, letGo);
  }

  /** Let go of every root a row's subtree owns, before the row is dropped. */
  release(row: Element): void {
    for (const el of row.querySelectorAll('*')) this.#release(el);
  }

  /** Let go of every root whose element has left the document. */
  sweep(): void {
    for (const host of this.#held.keys()) {
      if (!host.isConnected) this.#release(host);
    }
  }

  /** Let go of everything: the page that built the rows is going away. */
  releaseAll(): void {
    for (const letGo of this.#held.values()) letGo();
    this.#held.clear();
  }

  #release(host: Element): void {
    const letGo = this.#held.get(host);
    if (letGo === undefined) return;
    letGo();
    this.#held.delete(host);
  }
}
