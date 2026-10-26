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
 *
 * **A pass costs the whole subject again, so the bound is one pass, not a
 * beat.** A re-ask is a fresh subscribe, so the server re-reads, re-encodes
 * and re-sends the subject - 2.4-3.7 MB on a busy seat - and the new answer
 * queues behind the one already on its way. A fixed cadence therefore makes a
 * slow link slower in proportion to how slow it already is: measured on a
 * WireGuard client (#2561), a 5 s beat against answers taking 2-18 s sent 7-8
 * whole re-subscribes per busy seat visit and held the tunnel at
 * 0.13-0.34 MB/s. So a read gets one pass at [`READ_BOUND_MS`] and every check
 * after it waits [`SLOW_RETRY_MS`]: a lost answer is still found by the pass
 * at five seconds, and only a lost pass waits the beat.
 *
 * **The store is the state the bound reads**, so nothing else has to track
 * it: `ready` and `refused` are answers and only `loading` is a read still
 * owed, and an answer or a refusal is what ends the retries. A subject with
 * no store is asked for by nobody and is left alone - a visited seat keeps
 * its store, so a seat's read is bounded for the connection's life.
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
 * home encodes in 8-333 ms, measured across warm and cold reads - and long
 * enough that an answer merely crossing a slow link has a chance to land
 * before the read is presumed lost.
 */
export const READ_BOUND_MS = 5_000;

/**
 * How long an unanswered read waits between checks after its one pass.
 *
 * Sixty is more than three times the slowest whole-record answer measured on
 * the link behind #2561 (2-18 s), so the beat never lands on an answer still
 * travelling - the one pass at five seconds can - and a read that is genuinely
 * wedged costs one whole record a minute, a rounding error beside the storm it
 * replaces.
 *
 * **The trade is recovery time for a lost pass.** A lost answer is still found
 * by the pass at five seconds; if that pass is lost too, the next check is up
 * to 65 s away, where the fixed cadence asked at ten.
 *
 * **A reopen, not this beat, is the ordinary recovery.** The desktop drops
 * every timer on each `closed`, so a drop that reaches the page arms a fresh
 * pass on open. A transition it reads as `open` without ever reading `closed`
 * leaves a held timer standing, and a loading page then waits up to it.
 */
export const SLOW_RETRY_MS = 60_000;

export class ReadBounds {
  private readonly timers = new Map<string, ReturnType<typeof setTimeout>>();

  /**
   * `reask` is the connection's own refresh: the pair that asks for a
   * subject again without changing how many times it is watched, answering
   * whether the ask really went - a connection that is down or closed has
   * nothing to ask on, and a bound may not re-arm for a read it did not ask.
   */
  constructor(
    private readonly stores: Stores,
    private readonly reask: (what: Subject) => boolean,
    private readonly boundMs: number = READ_BOUND_MS,
    private readonly slowMs: number = SLOW_RETRY_MS,
  ) {}

  /**
   * A read just went out for `what`: bound it.
   *
   * **This arms the read's one pass at [`READ_BOUND_MS`].** Every check after
   * it waits [`SLOW_RETRY_MS`], which is the whole policy: an answer ends the
   * bound, and a read still unanswered after its pass is presumed slow.
   */
  arm(what: Subject): void {
    this.schedule(what, this.boundMs);
  }

  /** An answer landed for `what` - a snapshot, or a refusal in its own words. */
  clear(what: Subject): void {
    this.clearKey(subjectKey(what));
  }

  /** The same, for a caller holding the key a refusal arrives under. */
  clearKey(key: string): void {
    this.clearTimer(key);
  }

  /**
   * Re-arm the bound for every subject still waiting for its first answer and
   * holding no timer.
   *
   * **A connection that opens after the bound fired has nothing owed on it.**
   * The subscribe was queued while the socket was down and goes out when it
   * opens; if that first snapshot is lost, no timer remains to ask again. So
   * the connection calls this on open: a loading subject with no timer gets
   * the pass, and a subject already answered is left alone.
   *
   * **A timer that survived the transition is not shortened.** The desktop
   * drops every timer on each `closed`, so a reopen that reads `closed` then
   * `open` arms a fresh pass. A transition read as `open` alone leaves a held
   * timer standing, and a loading page can wait up to the beat; asking sooner
   * would buy an extra whole record per reconnect on exactly the slow link
   * this bound stopped feeding.
   */
  rearmLoading(): void {
    for (const subject of this.stores.subjects()) {
      const key = subjectKey(subject);
      if (this.timers.has(key)) continue;
      if (this.stores.get(subject)?.state().kind === 'loading') {
        this.arm(subject);
      }
    }
  }

  /**
   * Drop every timer: the connection this bound was made for is gone.
   *
   * The desktop calls this on each `closed`, which a transient drop reports
   * too, so the next open arms its subjects afresh.
   */
  stop(): void {
    for (const held of this.timers.values()) clearTimeout(held);
    this.timers.clear();
  }

  /** Arm one pass for `what`, `after` ms from now, replacing any timer held. */
  private schedule(what: Subject, after: number): void {
    const key = subjectKey(what);
    this.clearTimer(key);
    this.timers.set(
      key,
      setTimeout(() => this.expired(key, what), after),
    );
  }

  private clearTimer(key: string): void {
    const held = this.timers.get(key);
    if (held === undefined) return;
    clearTimeout(held);
    this.timers.delete(key);
  }

  private expired(key: string, what: Subject): void {
    this.timers.delete(key);
    const store = this.stores.get(what);
    if (store === undefined) return;
    if (store.state().kind !== 'loading') return;
    // Only a re-ask that really went leaves a read owed; re-arming for one
    // that could not go would tick forever for a subject nothing can answer,
    // and a closed connection's re-ask is exactly that case - which is what
    // keeps the chain from outliving its `stop` without a flag to say so.
    if (!this.reask(what)) return;
    // **A read still unanswered after its pass is slow rather than lost**, and
    // another whole record queued behind the answer already on its way would
    // only make it later (#2561).
    this.schedule(what, this.slowMs);
  }
}
