/**
 * The record's sub-agent cards, joined to the calls that opened them.
 *
 * **The record carries the instances; the conversation carries the rows.** A
 * card names the dispatch it belongs to (`dispatch_id`, the dispatch's own
 * `tool_use` id), and the chat's call row for that dispatch reads the card
 * here - one join, so a row and the record can never disagree about whether
 * an instance is still working. The CALL's own status cannot say that: the
 * launch-ack answers the dispatch in a second while the agent runs on.
 *
 * Held as the record is: one seat is on screen, so one map, replaced whole
 * when the record's list moves.
 */

import { SvelteMap } from 'svelte/reactivity';

import type { SubagentCard } from '../session/wire';

export class SubagentCards {
  #byDispatch = new SvelteMap<string, SubagentCard>();

  /** Take the seat's cards whole: what the record holds is what a row joins to. */
  sync(cards: readonly SubagentCard[] | null | undefined): void {
    this.#byDispatch.clear();
    for (const card of cards ?? []) this.#byDispatch.set(card.dispatch_id, card);
  }

  /** The card a dispatch opened, or `undefined` for a call that opened none. */
  by(dispatchId: string): SubagentCard | undefined {
    return this.#byDispatch.get(dispatchId);
  }
}

/** One per client, as the seat records are. */
export const subagents = new SubagentCards();
