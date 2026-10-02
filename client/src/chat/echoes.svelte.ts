/**
 * The reader's own words, from the moment enter is pressed to the moment the
 * core has them.
 *
 * **Both surfaces that send are one mechanism.** A prompt and a prompt's answer
 * are both a command that leaves the browser and returns no answer of its own,
 * so the wait has to be drawn somewhere or the box clears for no reason and a
 * slow dispatch reads as a key that did nothing. The mark rides the row the
 * words are going to occupy, which is where the reader is already looking.
 *
 * Held per seat, keyed the way the composer's box is: a reader moving between
 * seats must not meet another seat's pending send, and a send that fails must
 * still be there when they come back.
 *
 * The terminal is the reference and it has the same shape: it draws the
 * reader's words at submit rather than waiting for the core's own copy of
 * them, and it holds that per session.
 */

import { SvelteMap } from 'svelte/reactivity';

import { queuedWords } from './units';

/** One outstanding send: the words, and whether the core has refused them. */
export type Echo =
  { state: 'sending'; words: string } | { state: 'failed'; words: string; why: string };

/** A send the core has taken or refused leaves nothing behind, which is a delete rather than a third state. */
export class Echoes {
  #held = new SvelteMap<string, Echo>();

  /** What this seat is waiting on, or `undefined` when it is waiting on nothing. */
  of(key: string): Echo | undefined {
    return this.#held.get(key);
  }

  /**
   * Words on their way.
   *
   * A second send takes the first one's place: one row carries what the reader
   * is waiting on, and the first send's own row arrives from the wire as soon
   * as the core has it.
   */
  post(key: string, words: string): void {
    this.#held.set(key, { state: 'sending', words });
  }

  /**
   * The core refused it, in its own words.
   *
   * Only a send still on its way is refused here: a refusal that arrives after
   * the words landed belongs to another page's command, and drawing it would
   * put a stranger's failure in the reader's column.
   */
  refuse(key: string, why: string): void {
    const held = this.#held.get(key);
    if (held === undefined || held.state !== 'sending') return;
    this.#held.set(key, { state: 'failed', words: held.words, why });
  }

  /**
   * The seats waiting on a send, which is what a refusal is about.
   *
   * A refusal names the operation and never the seat (`{what: 'dispatch'}`),
   * so a send still on its way is the only thing it can be read against - and
   * a reader who sent and moved on still has to be told.
   */
  outstanding(): string[] {
    const out: string[] = [];
    for (const [key, echo] of this.#held) if (echo.state === 'sending') out.push(key);
    return out;
  }

  /**
   * The reader has left this seat, which gives up its send.
   *
   * A send still on its way belongs to the seat being looked at: the words are
   * the core's now and its own row arrives where they are going, so a mark
   * saying otherwise on a seat nobody is watching has nothing left to clear it
   * - the refusal would have been read against the seat on screen. What has
   * already failed stays: that row is the reader's, with the reason on it.
   */
  leave(key: string): void {
    for (const [held, echo] of this.#held) {
      if (held !== key && echo.state === 'sending') this.#held.delete(held);
    }
  }

  /** The core has the words, or the reader has given up on them. */
  clear(key: string): void {
    this.#held.delete(key);
  }
}

/** One per client, as the seat records are: a seat's pending send outlives the page drawing it. */
export const echoes = new Echoes();

function blocksOf(content: unknown): { type?: unknown; text?: unknown }[] {
  if (typeof content === 'string') return [{ type: 'text', text: content }];
  return Array.isArray(content) ? (content as { type?: unknown; text?: unknown }[]) : [];
}

/**
 * The reader's own words a frame carries, in either carrier the wire uses.
 *
 * A prompt that starts a turn arrives as the message's own text; one sent
 * while a turn is already running is held by the CLI as a `queued_command`
 * block instead. A reconcile that knew one of them would hang on the other,
 * and the send it hangs on is the one the reader is watching.
 */
export function ownWords(message: unknown): string[] {
  const frame = message as { type?: unknown; message?: { content?: unknown } } | null;
  const out: string[] = [];
  const queued = queuedWords(message);
  if (queued !== null) out.push(queued);
  if (frame?.type !== 'user') return out;
  for (const block of blocksOf(frame.message?.content)) {
    if (block.type === 'text' && typeof block.text === 'string') out.push(block.text);
  }
  return out;
}
