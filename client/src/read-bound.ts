/**
 * The bound on a subscription's first read.
 *
 * **A subscribe is answered or refused, and nothing in the protocol bounds
 * how long that takes.** Every re-ask the client makes is driven by
 * something ARRIVING - an update, a reconnect, a page's own refresh - so a
 * first snapshot lost on a live socket leaves the store loading for as long
 * as the connection lives: the page waits at "Reading the fleet..." with
 * nothing to ask again, on the phone over LAN and WireGuard alike, and only
 * a reconnect or a passing update ever heals it.
 *
 * So a subject still loading when the bound runs out is asked again. The
 * re-ask is the `refresh` shape - an unsubscribe and a subscribe - which is
 * what the server counts evenly, so a retry leaves no surplus watch behind.
 * The bound re-arms with the ask, or one loss would only ever buy one retry.
 *
 * **The store is the state the bound reads**, so nothing else has to track
 * it: `ready` and `refused` are answers and only `loading` is a read still
 * owed. A subject whose last reader let go is asked for by nobody and is left
 * alone.
 */

import type { Subject } from './protocol';
import { subjectKey } from './protocol';
import type { Stores } from './stores';

/**
 * How long a read may go unanswered before it is asked again.
 *
 * The same figure as the greeting's own deadline (`attempt.ts`'s
 * `HANDSHAKE_MS`), which is this rule one step earlier: a socket that opened
 * and then said nothing. Far above a healthy read - the live fleet's whole
 * home encodes in 8-333 ms, measured across warm and cold reads - and the
 * re-ask of a read that WAS merely slow costs one more encode rather than a
 * wrong answer.
 */
export const READ_BOUND_MS = 5_000;

export class ReadBounds {
  private readonly timers = new Map<string, ReturnType<typeof setTimeout>>();

  /**
   * `reask` is the connection's own refresh: the pair that asks for a
   * subject again without changing how many times it is watched.
   */
  constructor(
    private readonly stores: Stores,
    private readonly reask: (what: Subject) => void,
    private readonly boundMs: number = READ_BOUND_MS,
  ) {}

  /** A read just went out for `what`: bound it. */
  arm(what: Subject): void {
    this.clear(what);
    const key = subjectKey(what);
    this.timers.set(
      key,
      setTimeout(() => this.expired(key, what), this.boundMs),
    );
  }

  /** An answer landed for `what` - a snapshot, or a refusal in its own words. */
  clear(what: Subject): void {
    this.clearKey(subjectKey(what));
  }

  /** The same, for a caller holding the key a refusal arrives under. */
  clearKey(key: string): void {
    const held = this.timers.get(key);
    if (held === undefined) return;
    clearTimeout(held);
    this.timers.delete(key);
  }

  /** The connection is closed for good: nothing is owed on it any more. */
  stop(): void {
    for (const held of this.timers.values()) clearTimeout(held);
    this.timers.clear();
  }

  private expired(key: string, what: Subject): void {
    this.timers.delete(key);
    const store = this.stores.get(what);
    if (store === undefined) return;
    if (store.state().kind !== 'loading') return;
    this.reask(what);
    this.arm(what);
  }
}
