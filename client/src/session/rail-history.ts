/**
 * The covering rail's history step, as decisions rather than as clicks.
 *
 * **A rail that covers the page is an overlay, and an overlay takes a history
 * entry: Back closes what it opened.** A rail shown as a column at a wide
 * width is not an overlay and takes none. The projects pane is the one rail
 * left - the inspector's going took the other side with it - so a side is no
 * longer a value here. The client's tests carry no DOM renderer, so the push
 * and what a pop means live here, where they are assertable, and the page
 * calls them.
 */

/** The one rail left, which is what an entry of ours names. */
export type RailSide = 'left';

/** The entry an opening covering rail pushes, over whatever was there. */
export function railEntry(current: unknown): { forgeRail: RailSide } {
  const held = typeof current === 'object' && current !== null ? current : {};
  return { ...held, forgeRail: 'left' };
}

/** The side the entry on top opened, where it is one of ours. */
export function railOnTop(state: unknown): RailSide | null {
  const side = (state as { forgeRail?: unknown } | null)?.forgeRail;
  return side === 'left' ? side : null;
}

/**
 * What a pop means for the rail, `null` for a pop that says nothing about it.
 *
 * Landing on the rail's entry shows it, and an entry whose rail was closed
 * while another navigation sat on top of it (`closedUnder`) shows nothing:
 * the press that walks past it cannot re-open what a close already closed.
 * Landing anywhere else closes the rail where it covers, and at a wide width
 * closes it only where the pop left its entry (`leftRail`) - the band opening
 * back up under an open rail - so that press still closes what it opened.
 */
export function chosenAfterPop(
  state: unknown,
  narrow: boolean,
  leftRail: RailSide | null,
  closedUnder: RailSide | null,
): { left: boolean | null } | null {
  const side = railOnTop(state);
  if (side !== null) {
    return { left: side !== closedUnder };
  }
  if (narrow) return { left: false };
  if (leftRail === 'left') return { left: false };
  return null;
}
