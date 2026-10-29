/**
 * A published change, in the words both surfaces use.
 *
 * **The chat row and the strip note name one provider the same way.** They
 * are separate components, so a copy of the noun mapping in each is a
 * one-sided edit away from two names for one change.
 */

/** `PR`, `MR`, or `change`, from the provider the frame names. */
export function changeNoun(provider: string | null): string {
  return provider === 'gitlab'
    ? 'MR'
    : provider?.startsWith('github') === true || provider === 'bitbucket'
      ? 'PR'
      : 'change';
}

/** The CLI's action in the strip's own words; an unknown one as the wire's. */
export function changeVerb(action: string | null): string {
  switch (action) {
    case null:
    case '':
      return 'changed';
    case 'checked-out':
      return 'checked out';
    case 'auto-merge-enabled':
      return 'auto-merge on';
    case 'auto-merge-disabled':
      return 'auto-merge off';
    case 'ready':
      return 'marked ready';
    case 'draft':
      return 'marked draft';
    default:
      return action;
  }
}

/**
 * The strip's line for a published change: the action the CLI announced, on
 * the change it names. Every action gets one - the four that keep a chat row
 * and the ones that do not alike - so a comment, an edit or a checkout is not
 * silent (rule 25).
 */
export function changeLine(
  provider: string | null,
  identifier: string | null,
  action: string | null,
  repo: string | null,
): string {
  const noun = changeNoun(provider);
  const named = identifier === null || identifier === '' ? noun : `${noun} #${identifier}`;
  const did = changeVerb(action);
  return repo === null || repo === '' ? `${named} ${did}` : `${named} ${did} in ${repo}`;
}
