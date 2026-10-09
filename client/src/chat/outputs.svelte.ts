/**
 * The output a call's read answered with, held until the conversation that
 * asked for it goes.
 *
 * **Keyed by the call's own id, and deliberately not by the turn.** The
 * answer is asked for by the row that opened (`Call.svelte`), which knows
 * the id it asked with and nothing about the seat - and a call id is unique
 * the way a tool_use id is, so the seat is carried beside the answer rather
 * than in the key, which is what lets a swap drop exactly the going
 * occupant's answers.
 *
 * Held here rather than folded into the conversation because no page carries
 * it: it is the answer to an ask, not a frame of the transcript, so a resume
 * re-asks rather than re-reading it.
 */

import { SvelteMap } from 'svelte/reactivity';

import type { CallOutput } from '../wire/call-output';

/** One answer, with the seat that asked for it. */
interface Held {
  seat: string;
  output: CallOutput;
}

/** One per client, as the seat records are: an answer outlives the row drawing it. */
export class Outputs {
  #held = new SvelteMap<string, Held>();

  /** What the read for `callId` answered, or `undefined` while none has. */
  of(callId: string): CallOutput | undefined {
    return this.#held.get(callId)?.output;
  }

  /** Record an answer for a call of `seat`. A later answer takes the old one's place. */
  post(seat: string, callId: string, output: CallOutput): void {
    this.#held.set(callId, { seat, output });
  }

  /** A conversation is gone - a swap replaced its occupant: its answers go with it. */
  clear(seat: string): void {
    for (const [callId, held] of [...this.#held]) {
      if (held.seat === seat) this.#held.delete(callId);
    }
  }
}

export const outputs = new Outputs();
