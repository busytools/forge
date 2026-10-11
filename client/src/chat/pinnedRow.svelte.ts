/**
 * The turn's metadata row a seat is currently pinning, for the strip lane the
 * composer draws.
 *
 * **One fact with two readers, and this is the wire between them.** The row is
 * folded from the conversation's units, which only the chat column holds, and
 * it is drawn in the foot surface the composer owns - so the chat publishes
 * what it pins and the composer reads it, rather than either rebuilding the
 * fold. The chat is the only writer.
 *
 * Held per seat, keyed the way the composer's box is: a seat switch swaps the
 * whole surface, and another seat's running figures must never draw on it.
 *
 * The name is `pinnedRow` rather than `pinned` on purpose: a store beside the
 * `Pinned.svelte` component would collide with it on a case-insensitive
 * filesystem, and `./pinned.svelte` would resolve to the component.
 */

import { SvelteMap } from 'svelte/reactivity';

import type { TurnInfo } from './units';

export class PinnedRow {
  #held = new SvelteMap<string, TurnInfo>();

  /** The row this seat pins, or `null` when it pins nothing. */
  of(key: string): TurnInfo | null {
    return this.#held.get(key) ?? null;
  }

  /** What the chat publishes: the pinned row, or `null` when there is none. */
  set(key: string, info: TurnInfo | null): void {
    if (info === null) this.#held.delete(key);
    else this.#held.set(key, info);
  }
}

/** One per client, as the seat records are: a pin outlives the page drawing it. */
export const pinnedRow = new PinnedRow();
