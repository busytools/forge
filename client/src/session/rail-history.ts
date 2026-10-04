/**
 * The covering rail's history step, as decisions rather than as clicks.
 *
 * **A rail that covers the page is an overlay, and an overlay takes a history
 * entry: Back closes what it opened.** A rail shown as a column at a wide
 * width is not an overlay and takes none. The client's tests carry no DOM
 * renderer, so the push and what a pop means live here, where they are
 * assertable, and the page calls them.
 */

export type RailSide = 'left' | 'right';

/** The entry an opening covering rail pushes, over whatever was there. */
export function railEntry(current: unknown, side: RailSide): { forgeRail: RailSide } {
  const held = typeof current === 'object' && current !== null ? current : {};
  return { ...held, forgeRail: side };
}

/** The side the entry on top opened, where it is one of ours. */
export function railOnTop(state: unknown): RailSide | null {
  const side = (state as { forgeRail?: unknown } | null)?.forgeRail;
  return side === 'left' || side === 'right' ? side : null;
}

/**
 * What a pop means for the two choices.
 *
 * Landing on a side's entry shows that side and steps out of the other; a pop
 * that lands anywhere else closes both, and only where a rail covers, so a
 * route step on a wide page leaves the columns as they were. `null` means the
 * pop says nothing about them.
 */
export function chosenAfterPop(
  state: unknown,
  narrow: boolean,
): { left: boolean | null; right: boolean | null } | null {
  const side = railOnTop(state);
  if (side === 'left') return { left: true, right: false };
  if (side === 'right') return { left: false, right: true };
  return narrow ? { left: false, right: false } : null;
}
