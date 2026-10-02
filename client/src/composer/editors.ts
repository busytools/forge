/**
 * Every box in the client that can take text, and the table that says which of
 * them a take's words belong to.
 *
 * Derived, never stored: nothing registers here, so nothing can go stale and
 * there is no mount order to get wrong.
 *
 * It is not yet the whole of the keyboard's routing. The composer and the dock
 * still move the caret in their own effects - the connect screen has none - and
 * the landing is the one caller this table has, so a surface added here does not
 * have its keyboard follow by itself.
 *
 * Every box that can take text is one of these, and `editors.test.ts` sweeps the
 * tree for one that forgot to say so.
 */

/**
 * The boxes that can take text. A closed set, so a new one cannot be forgotten -
 * and a value rather than a bare type, so the census can assert membership in it
 * rather than a box's word for itself.
 */
export const EDITORS = ['composer', 'dock', 'connect', 'nowhere'] as const;

export type Editor = (typeof EDITORS)[number];

/** What the page knows that decides the keyboard, and nothing else. */
export type Where = {
  /** What the reader last acted on, or what was remembered for them. */
  editor: Editor;
  /** What held the keyboard when the current prompt arrived. */
  remember: Editor;
  /** Whether a prompt is waiting for an answer. */
  pending: boolean;
  /** Whether each route's box is on screen at all. */
  composerPresent?: boolean;
  connectPresent?: boolean;
  /**
   * Whether the dock's own box - the prompt's free-text row - is open.
   *
   * A prompt holds the keyboard whether or not that row is open, but it can only
   * take a take's words when the reader has opened the box to put them in.
   */
  dockPresent?: boolean;
};

/**
 * Which box the reader's words belong to.
 *
 * A prompt takes it and holds it while the queue drains; anything else leaves it
 * where the reader put it. The remembered surface can be gone - a prompt answered
 * after the route changed - so a name that is no longer mounted resolves to a
 * surface that is, never to a box that is not there.
 */
export function focusOf(where: Where): Editor {
  if (where.pending) return where.dockPresent === false ? fallback(where) : 'dock';
  const named = where.editor === 'dock' ? where.remember : where.editor;
  switch (named) {
    case 'dock':
      return 'dock';
    case 'connect':
      return where.connectPresent === false ? fallback(where) : 'connect';
    case 'composer':
      return where.composerPresent === false ? 'nowhere' : 'composer';
    case 'nowhere':
      return 'nowhere';
    default: {
      // A new Editor with no case here fails the build, which is what the
      // closed set is for.
      const unhandled: never = named;
      throw new Error(`no editor named ${String(unhandled)}`);
    }
  }
}

function fallback(where: Where): Editor {
  return where.composerPresent === false ? 'nowhere' : 'composer';
}
