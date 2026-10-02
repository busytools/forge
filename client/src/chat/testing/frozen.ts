/**
 * The conversation a column is HANDED, frozen, for the tests that drive one.
 *
 * **Every publish is frozen, at the boundary the class passes through** - the
 * record the column holds and the record the class keeps are the same object,
 * so a test that never reads `value` is covered too. That is the boundary that
 * matters because the column holds its record as a VALUE: `$state.raw` never
 * notifies on a publish of the same identity, so an in-place edit made where a
 * record should have been REPLACED is silent - the previous occupant's history
 * stays on screen through a `/new` - and freezing turns that silence into a
 * throw.
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
