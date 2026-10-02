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
  what: 'compaction';
  token: number;
}

export const scrollAsk = writable<ScrollAsk | null>(null);

/** Ask the column to reveal its latest compaction. */
export function askCompaction(): void {
  scrollAsk.set({ what: 'compaction', token: Date.now() });
}
