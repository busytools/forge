/**
 * The seat's whole tree, as the strip's files slip draws it: the path the
 * panel names, and the tree's own NAME when it is not the project's (a
 * worker's worktree) - the slip itself carries no count (Ved, 2026-10-17),
 * because what a reader needs there is WHICH tree the door opens.
 *
 * Held as the record is: one seat is on screen, so one slip, replaced whole
 * when the record moves. **The slip is mandatory** (Ved, 2026-10-17): every
 * seat has a tree, so this draws wherever a record exists, where the git row
 * beside it stays away when nothing has moved.
 */

class Files {
  #tree = $state<string | null>(null);
  #name = $state<string | null>(null);

  /** Take the record's read whole: the tree's resolved path, and its own
   *  name when the seat works in one that is not its project's. */
  sync(tree: string | null, name: string | null): void {
    this.#tree = tree;
    this.#name = name;
  }

  /** Whether there is a record to draw the slip for at all. */
  anything(): boolean {
    return this.#tree !== null;
  }

  /** The tree's own path, as the record resolved it - the panel's line. */
  tree(): string | null {
    return this.#tree;
  }

  /** The worktree's name, or `null` when the tree is the project's own. */
  name(): string | null {
    return this.#name;
  }
}

/** The one seat's tree, as its record last stated it. */
export const files = new Files();
