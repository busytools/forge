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

import { SvelteMap, SvelteSet } from 'svelte/reactivity';

import type { SubagentCard } from '../session/wire';

export class SubagentCards {
  #byDispatch = new SvelteMap<string, SubagentCard>();
  #list = $state<SubagentCard[]>([]);
  /**
   * The dispatches whose ROWS the page holds, off the loaded turns.
   *
   * The record's list is the session's whole history; the conversation is a
   * window of it, and the transport floors how far back the window pages. An
   * instance the record holds but the page cannot reach would make a list
   * entry that clicks through to nothing, so the chat sets this and the list
   * reads it.
   */
  #reachable = $state<ReadonlySet<string>>(new SvelteSet());

  /** Take the seat's cards whole: what the record holds is what a row joins to. */
  sync(cards: readonly SubagentCard[] | null | undefined): void {
    const next = [...(cards ?? [])];
    this.#list = next;
    this.#byDispatch.clear();
    for (const card of next) this.#byDispatch.set(card.dispatch_id, card);
  }

  /** The dispatches the loaded conversation can actually reach. */
  syncReachable(ids: ReadonlySet<string>): void {
    this.#reachable = ids;
  }

  /** Whether the page holds the row this dispatch drew. */
  reachable(dispatchId: string): boolean {
    return this.#reachable.has(dispatchId);
  }

  /** The card a dispatch opened, or `undefined` for a call that opened none. */
  by(dispatchId: string): SubagentCard | undefined {
    return this.#byDispatch.get(dispatchId);
  }

  /** Every instance the record holds, in dispatch order. */
  all(): SubagentCard[] {
    return this.#list;
  }
}

/**
 * The instances a list may lead to: one that is running, and whose row the
 * page holds. A running instance's row is still in the live turn; a settled
 * one's is only reachable when the conversation carries its turn - an instance
 * from before a restart or resume has no frames held, and one beyond the
 * transport's floor cannot be paged back to, so neither leads anywhere.
 */
export function transcribable(
  cards: readonly SubagentCard[],
  reachable: (dispatchId: string) => boolean,
): SubagentCard[] {
  return cards.filter((card) => card.running || reachable(card.dispatch_id));
}

/**
 * The dispatches the loaded turns hold: every top-level `tool_use` id, which
 * is the join key a dispatch row's data attribute carries. A frame from a
 * dispatched agent is skipped - its calls live inside a row, not as rows of
 * their own, so none of them is a dispatch the list could lead to.
 */
export function reachableIds(
  turns: readonly { messages: readonly unknown[] }[],
): ReadonlySet<string> {
  const ids = new SvelteSet<string>();
  for (const turn of turns) {
    for (const message of turn.messages) {
      const frame = message as {
        parent_tool_use_id?: unknown;
        message?: { content?: unknown };
      };
      const parent = frame.parent_tool_use_id;
      if (typeof parent === 'string' && parent !== '') continue;
      const content = frame.message?.content;
      if (!Array.isArray(content)) continue;
      for (const block of content) {
        const held = block as { type?: unknown; id?: unknown };
        if (held.type === 'tool_use' && typeof held.id === 'string') ids.add(held.id);
      }
    }
  }
  return ids;
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
  flash(row);
  return true;
}

/**
 * The one-flash, lit again when the scroll LANDS.
 *
 * A long smooth scroll outlives a flash lit at its start, so the reader
 * arrives at a row that has already stopped signalling. `scrollend` is the
 * precise signal; the timer covers engines without it.
 */
function flash(row: HTMLDetailsElement): void {
  const light = () => {
    row.classList.remove('sg-hit');
    void row.offsetWidth;
    row.classList.add('sg-hit');
  };
  light();
  let landed = false;
  const land = () => {
    if (landed) return;
    landed = true;
    light();
    setTimeout(() => row.classList.remove('sg-hit'), 1700);
  };
  const doc = row.ownerDocument;
  if (doc != null && typeof doc.addEventListener === 'function') {
    // Capture, so any descendant scroll is seen - but only the scroller the
    // ROW rides: another scroller's end would re-light this flash early and
    // eat the one-shot.
    const onEnd = (event: Event) => {
      const target = event.target;
      if (!(target instanceof Node) || !target.contains(row)) return;
      doc.removeEventListener('scrollend', onEnd, { capture: true });
      land();
    };
    doc.addEventListener('scrollend', onEnd, { capture: true });
  }
  setTimeout(land, 700);
}

/** One per client, as the seat records are. */
export const subagents = new SubagentCards();
