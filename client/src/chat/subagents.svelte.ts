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
  #list = $state<SubagentCard[]>([]);

  /** Take the seat's cards whole: what the record holds is what a row joins to. */
  sync(cards: readonly SubagentCard[] | null | undefined): void {
    const next = [...(cards ?? [])];
    this.#list = next;
    this.#byDispatch.clear();
    for (const card of next) this.#byDispatch.set(card.dispatch_id, card);
  }

  /** The card a dispatch opened, or `undefined` for a call that opened none. */
  by(dispatchId: string): SubagentCard | undefined {
    return this.#byDispatch.get(dispatchId);
  }

  /** Every instance the record holds, in dispatch order. */
  all(): SubagentCard[] {
    return this.#list;
  }

  /** How many of them are still working. */
  running(): number {
    return this.#list.filter((card) => card.running).length;
  }
}

/**
 * The instances a transcript can be opened for: one that is running, or one
 * with calls on the page. An instance that ran before a restart or resume has
 * no frames held, so its row would open onto a brief and nothing - it stays
 * out of any list that leads somewhere.
 */
export function transcribable(cards: readonly SubagentCard[]): SubagentCard[] {
  return cards.filter((card) => card.running || card.calls > 0);
}

/**
 * A list read newest first: the instance a reader just watched start is the
 * one they came to the list for, and oldest-first buries it below the fold of
 * a long session.
 */
export function latestFirst(cards: readonly SubagentCard[]): SubagentCard[] {
  return [...cards].reverse();
}

/**
 * Reveal the chat row a dispatch drew: open it, bring it into view, and give
 * it one flash so the eye lands on it.
 *
 * **Focus is left alone** - the reader keeps whatever they were typing - and a
 * row the page does not hold answers false rather than throwing.
 */
export function reveal(dispatchId: string, root: ParentNode = document): boolean {
  // Escaped, because a wire id is interpolated into a selector and one with a
  // quote or a backslash in it would throw rather than answer.
  const escaped = dispatchId.replace(/["\\]/g, '\\$&');
  const row = root.querySelector<HTMLDetailsElement>(`details[data-sg="${escaped}"]`);
  if (row === null) return false;
  row.open = true;
  row.scrollIntoView({ behavior: 'smooth', block: 'center' });
  row.classList.remove('sg-hit');
  void row.offsetWidth;
  row.classList.add('sg-hit');
  setTimeout(() => row.classList.remove('sg-hit'), 1700);
  return true;
}

/** One per client, as the seat records are. */
export const subagents = new SubagentCards();
