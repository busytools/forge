/**
 * Every box in the client that can take text, and the one function that says
 * which of them holds the keyboard.
 *
 * Focus is derived, never stored: nothing registers here, so nothing can go
 * stale and there is no mount order to get wrong.
 *
 * Every box that can take text is one of these, and `editors.test.ts` sweeps the
 * tree for one that forgot to say so.
 */

/** The boxes that can take text. A closed set, so a new one cannot be forgotten. */
export type Editor = 'composer' | 'dock' | 'connect' | 'nowhere';

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
};

/**
 * Which box holds the keyboard.
 *
 * A prompt takes it and holds it while the queue drains; anything else leaves it
 * where the reader put it. The remembered surface can be gone - a prompt answered
 * after the route changed - so a name that is no longer mounted resolves to a
 * surface that is, never to a box that is not there.
 */
export function focusOf(where: Where): Editor {
  if (where.pending) return 'dock';
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
