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
 * What a pop means for the two choices, `null` for a choice the pop says
 * nothing about.
 *
 * Landing on a side's entry shows that side, and steps out of the other where
 * a rail covers; at a wide width that other one is a column and is left as it
 * was. An entry whose rail was closed while another navigation sat on top of
 * it (`closedUnder`) shows nothing: the press that walks past it cannot
 * re-open what a close already closed. Landing anywhere else closes both where
 * a rail covers, and at a wide width closes only the side whose entry the pop
 * left (`leftRail`) - the band opening back up under an open rail - so that
 * press still closes what it opened.
 */
export function chosenAfterPop(
  state: unknown,
  narrow: boolean,
  leftRail: RailSide | null,
  closedUnder: RailSide | null,
): { left: boolean | null; right: boolean | null } | null {
  const side = railOnTop(state);
  if (side !== null) {
    const shown = side !== closedUnder;
    const otherCloses = narrow ? false : null;
    return side === 'left'
      ? { left: shown, right: otherCloses }
      : { left: otherCloses, right: shown };
  }
  if (narrow) return { left: false, right: false };
  if (leftRail === 'left') return { left: false, right: null };
  if (leftRail === 'right') return { left: null, right: false };
  return null;
}
