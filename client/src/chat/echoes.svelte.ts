/**
 * The reader's own words, from the moment enter is pressed to the moment the
 * core has them.
 *
 * **One entry per message, keyed by the prompt's own id**, because the pile
 * draws every one of them: a second press while the first still waits is a
 * second card, and two sends of the same text are told apart by id, never by
 * their words.
 *
 * The pending row is the ONLY home the words have until the core answers: the
 * queue draws it as `sending`, the core's `prompt_queued` hands it over to the
 * core's own row, and its `sent` frame moves it into the chat. A refusal keeps
 * it where it is with the reason and a retry, which is the whole of this
 * client's refusal channel for a send.
 *
 * Held per seat, keyed the way the composer's box is: a reader moving between
 * seats must not meet another seat's pending send, and a send that fails must
 * still be there when they come back.
 */

import { SvelteMap } from 'svelte/reactivity';

import { queuedWords } from './units';

/** One send, from the enter that started it to the core's own word about it. */
export interface PendingSend {
  seat: string;
  uuid: string;
  source: string;
  words: string;
  /**
   * `sending` until the core answers, then gone; `not_sent` when it never
   * went, carrying the reason and the retry the sender is owed.
   */
  state: 'sending' | 'not_sent';
  /** Why it is not sent: the core's words where it refused, ours where a link died. */
  reason: string;
  /**
   * When the transport accepted the frame, in milliseconds, or `null` while
   * it has not. The 30 s `no word yet` is measured from here: acceptance is
   * the last moment anything at this end can vouch for the send.
   */
  acceptedAt: number | null;
  /** Whether the seat already had a turn in flight when the words were posted. */
  running: boolean;
  dispatch: number;
}

/** The id a send and its seat make, one entry per message. */
function heldKey(seat: string, uuid: string): string {
  return `${seat}\u0000${uuid}`;
}

class Echoes {
  #held = new SvelteMap<string, PendingSend>();
  /**
   * The number this client's next press carries.
   *
   * **The refusal names it back**, so one seat's failed send is not read as
   * another's: a refusal echoes the dispatch id the command carried, and the
   * composer marks the send that minted it (#1583).
   */
  #next = 0;

  /** Every pending row a seat draws, oldest first. */
  of(seat: string): PendingSend[] {
    const out: PendingSend[] = [];
    for (const held of this.#held.values()) if (held.seat === seat) out.push(held);
    return out;
  }

  /** Whether this id is a send this client made for the seat. */
  isPending(seat: string, uuid: string): boolean {
    return this.#held.has(heldKey(seat, uuid));
  }

  /**
   * Words on their way.
   *
   * A refused row the last press left is retired here: the reader pressing
   * again has answered it, and the line where the card was would otherwise
   * stand beside a fresh send.
   */
  post(
    seat: string,
    uuid: string,
    words: string,
    running: boolean,
    dispatch: number,
    source = 'you',
  ): void {
    for (const [key, held] of this.#held) {
      if (held.seat === seat && held.state === 'not_sent') this.#held.delete(key);
    }
    this.#held.set(heldKey(seat, uuid), {
      seat,
      uuid,
      source,
      words,
      state: 'sending',
      reason: '',
      acceptedAt: null,
      running,
      dispatch,
    });
  }

  /** A number for one press, minted before its command goes. */
  mint(): number {
    this.#next += 1;
    return this.#next;
  }

  /**
   * The transport took the frame, which starts the 30 s clock.
   *
   * **Acceptance is not the core's word.** It says the frame crossed this
   * end's link; a core that never answers is what `no word yet` names.
   */
  accept(seat: string, uuid: string, at: number): void {
    const held = this.#held.get(heldKey(seat, uuid));
    if (held === undefined || held.acceptedAt !== null) return;
    this.#held.set(heldKey(seat, uuid), { ...held, acceptedAt: at });
  }

  /**
   * The core refused the press that named this dispatch, in its own words.
   *
   * Returns whether this client holds a send for that dispatch: the frame is
   * that send's whatever state it is in, and a refusal must not be read
   * against another seat's send.
   */
  refuseDispatch(dispatch: number, why: string): boolean {
    for (const held of this.#held.values()) {
      if (held.dispatch !== dispatch) continue;
      this.refuse(held.seat, held.uuid, why);
      return true;
    }
    return false;
  }

  /**
   * The send did not go, in the words the reader needs.
   *
   * Only one still on its way is refused here: a refusal that arrives after
   * the core took the words belongs to another command, and drawing it would
   * put a stranger's failure in the reader's column.
   */
  refuse(seat: string, uuid: string, why: string): void {
    const key = heldKey(seat, uuid);
    const held = this.#held.get(key);
    if (held === undefined || held.state !== 'sending') return;
    this.#held.set(key, { ...held, state: 'not_sent', reason: why });
  }

  /** Every send still on its way in `seat`, refused in the given words. */
  refuseAll(seat: string, why: string): void {
    for (const held of [...this.#held.values()]) {
      if (held.seat === seat) this.refuse(held.seat, held.uuid, why);
    }
  }

  /**
   * The core has them: its `sent` frame moved the row into the chat.
   *
   * The pending row goes; the chat row that the same moment draws is the
   * send's next and only home.
   */
  take(seat: string, uuid: string): void {
    this.#held.delete(heldKey(seat, uuid));
  }

  /**
   * A turn the seat started, so nothing still says `sending`.
   *
   * The backstop for a CLI that reports no lifecycle frames: no `sent` is
   * coming, and the row the dispatch drew is the words' home from here.
   *
   * **Only a send that was posted idle is taken.** A send made into a turn
   * already running did not start it, so the turn in flight is not its
   * answer - taking it there would drop the row a refusal is still owed to.
   */
  takeAll(seat: string): void {
    for (const [key, held] of this.#held) {
      if (held.seat === seat && held.state === 'sending' && !held.running) {
        this.#held.delete(key);
      }
    }
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
    for (const held of this.#held.values()) {
      if (held.state === 'sending' && !out.includes(held.seat)) out.push(held.seat);
    }
    return out;
  }

  /** The core has the words, or the reader has given up on them. */
  clear(seat: string, uuid: string): void {
    this.#held.delete(heldKey(seat, uuid));
  }

  /** Every pending row a seat holds, refused or not: a seat released, or a test. */
  clearSeat(seat: string): void {
    for (const [key, held] of this.#held) if (held.seat === seat) this.#held.delete(key);
  }

  /**
   * The prompt ids this client minted for `key`, whether or not they still
   * carry a pending row.
   *
   * **A send can leave from a surface that draws no row of its own** - an
   * answer to a takeover, a palette command, a review or document nudge - and
   * those post no row: the id is the only thing that says the words are this
   * client's, and a parked reader's own send must carry them to the foot
   * instead of counting in the badge. Bounded, because a long session mints
   * one per send and only the newest can still be in flight.
   */
  #minted = new Map<string, string[]>();
  remember(key: string, id: string): void {
    const held = this.#minted.get(key) ?? [];
    if (held.includes(id)) return;
    held.push(id);
    if (held.length > 64) held.shift();
    this.#minted.set(key, held);
  }

  /** Whether `id` is a prompt this client minted for `key`. */
  owns(key: string, id: string): boolean {
    return this.#minted.get(key)?.includes(id) ?? false;
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
