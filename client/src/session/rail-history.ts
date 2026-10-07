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

/** What an entry of ours opened: the projects rail, or the palette. */
export type RailSide = 'left' | 'palette';

/** The entry an opening overlay pushes, over whatever was there. */
export function railEntry(current: unknown, side: RailSide = 'left'): { forgeRail: RailSide } {
  const held = typeof current === 'object' && current !== null ? current : {};
  return { ...held, forgeRail: side };
}

/** What the entry on top opened, where it is one of ours. */
export function railOnTop(state: unknown): RailSide | null {
  const side = (state as { forgeRail?: unknown } | null)?.forgeRail;
  return side === 'left' || side === 'palette' ? side : null;
}

/**
 * What a pop means for the rail, `null` for a pop that says nothing about it.
 *
 * Landing on the rail's entry shows it, and an entry whose rail was closed
 * while another navigation sat on top of it (`closedUnder`) shows nothing:
 * the press that walks past it cannot re-open what a close already closed.
 * Landing anywhere else closes it where its entry was left (`leftRail`) -
 * the summoned rail covers, so that press still closes what it opened.
 */
export function chosenAfterPop(
  state: unknown,
  leftRail: RailSide | null,
  closedUnder: RailSide | null,
): { left: boolean | null } | null {
  const side = railOnTop(state);
  // A palette landing says nothing about the rail; the page closes the
  // palette on it and asks no further question here.
  if (side === 'left') {
    return { left: side !== closedUnder };
  }
  if (side === null && leftRail === 'left') return { left: false };
  return null;
}
