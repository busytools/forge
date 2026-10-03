/**
 * The conversation a column is HANDED, frozen, for the tests that drive one.
 *
 * **Every PUBLISH is frozen, at the boundary the class passes through.** Since
 * the fold state and the published store were split (#1670), the boundary is
 * the draw rather than the fold: a stream frame is folded at once and drawn on
 * the next painted frame, so the record a reader - and the column - can hold
 * is the published one, and that is the one frozen. A state between two folds
 * of one painted frame is not frozen until something publishes it - and a
 * reader arriving mid-burst flushes the fold as it stands, which IS a publish,
 * so the in-between record it is handed is frozen like any other.
 * The freeze still covers what matters: the column holds its record as a
 * VALUE, and `$state.raw` never notifies on a publish of the same identity, so
 * an in-place edit made where a record should have been REPLACED is silent -
 * the previous occupant's history stays on screen through a `/new` - and
 * freezing the published record turns that silence into a throw.
 */

/** The conversation module with a `Chat` that freezes every record it publishes. */
export function frozenConversation(
  real: typeof import('../conversation'),
): typeof import('../conversation') {
  class FrozenChat extends real.Chat {
    constructor(...args: ConstructorParameters<typeof real.Chat>) {
      super(...args);
      // Subscribing is what reaches the class's own record: a writable hands
      // its current value to a new subscriber, and every later publish comes
      // through here before any reader sees it.
      this.value.subscribe((value) => freeze(value));
    }
  }
  return { ...real, Chat: FrozenChat };
}

/** Every object in a value, frozen, so a write into it throws in strict mode. */
export function freeze<T>(value: T): T {
  if (value !== null && typeof value === 'object') {
    for (const held of Object.values(value as Record<string, unknown>)) freeze(held);
    Object.freeze(value);
  }
  return value;
}
