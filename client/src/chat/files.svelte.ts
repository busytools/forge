/**
 * The seat's whole tree, as the strip's files slip draws it: how many files
 * it holds, and which tree it is.
 *
 * Held as the record is: one seat is on screen, so one slip, replaced whole
 * when the record moves. **The slip is mandatory** (Ved, 2026-10-17): every
 * seat has a tree, so this draws wherever a record exists, where the git row
 * beside it stays away when nothing has moved.
 */

class Files {
  #count = $state<number | null>(null);
  #tree = $state<string | null>(null);

  /** Take the record's read whole. */
  sync(count: number | null, tree: string | null): void {
    this.#count = count;
    this.#tree = tree;
  }

  /** Whether there is a record to draw the slip for at all. */
  anything(): boolean {
    return this.#count !== null;
  }

  count(): number | null {
    return this.#count;
  }

  /** The tree's own path, as the record resolved it. */
  tree(): string | null {
    return this.#tree;
  }
}

/** The one seat's file count and tree, as its record last stated them. */
export const files = new Files();
