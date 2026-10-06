import { writable } from 'svelte/store';

/**
 * A request for the conversation column to move itself somewhere.
 *
 * The header owns the trigger and the column owns the scroll, and they are
 * separate component trees - so the ask travels as a store rather than as a
 * prop. The token is what makes a REPEAT land: asking twice for the same
 * place is a real ask the second time, and a value compared by its `what`
 * alone would be dropped as unchanged.
 */
export interface ScrollAsk {
  what: 'compaction' | 'dispatch';
  token: number;
  /** The call to reveal, for `what: 'dispatch'` - a dispatch or any tool
   * call with its own row: it may sit in a turn the virtualised list has not
   * drawn, so the column scrolls first. */
  call?: string;
}

export const scrollAsk = writable<ScrollAsk | null>(null);

/** Ask the column to reveal its latest compaction. */
export function askCompaction(): void {
  scrollAsk.set({ what: 'compaction', token: Date.now() });
}

/** Ask the column to reveal the chat row a call drew, by its tool_use id. */
export function askReveal(callId: string): void {
  scrollAsk.set({ what: 'dispatch', token: Date.now(), call: callId });
}
