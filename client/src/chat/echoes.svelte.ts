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

/**
 * One send, from the enter that started it to the core's own copy of the words.
 *
 * **Three states, because the question the row answers changes halfway.** At
 * first it is "has the core got this", which the turn going in flight answers.
 * After that it is "where are my words", and that one is not answered until the
 * core's own copy of them is in the conversation - which arrives on the next
 * page read rather than as a frame, since a prompt forge injects is not echoed
 * back on stream-json. A row that went with the first answer would take the
 * reader's words off the screen for the length of the turn.
 */
export type Echo =
  /**
   * On its way, and what the seat was doing when it was posted.
   *
   * `running` records whether a turn was already in flight at that moment: a
   * turn that was already there says the core had accepted something else, so
   * the next turn going in flight is not this send being taken. A send wrongly
   * read as taken cannot be refused any more - `refuse` rewrites only what is
   * still on its way - so the refusal would be lost in silence.
   */
  | { state: 'sending'; words: string; running: boolean }
  | { state: 'taken'; words: string }
  | { state: 'failed'; words: string; why: string };

/** A send the core's copy of has arrived leaves nothing behind, which is a delete rather than a fourth state. */
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
   *
   * `running` is whether the seat already had a turn in flight when the send
   * left, which is what tells a send that started a turn from one that landed
   * in a turn already running.
   */
  post(key: string, words: string, running: boolean): void {
    this.#held.set(key, { state: 'sending', words, running });
  }

  /**
   * The core has them: it started the turn they asked for.
   *
   * The mark goes and the words stay. This is the whole of "it is sent" - the
   * row is the reader's own message from here until the conversation carries
   * its own copy of it.
   *
   * Only a send that STARTED the turn is taken here. One posted into a turn
   * already running is settled by the conversation carrying its words, or by a
   * refusal - never by a turn it did not begin, which would take it past the
   * point where a refusal can still reach it.
   */
  take(key: string): void {
    const held = this.#held.get(key);
    if (held === undefined || held.state !== 'sending' || held.running) return;
    this.#held.set(key, { state: 'taken', words: held.words });
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
