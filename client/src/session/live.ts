/**
 * The session over a live connection: the record it is answered with, and the
 * update it applies to it.
 *
 * **The update is the news, and it is already everything a page needs.** The
 * view this client replaced applied each variant to the state it held, which
 * is why it kept up with a busy seat that this page used to fall behind:
 * answering every update with a read is a full encode on the server - the
 * transcript re-folded into turns, the process tree walked, the working tree
 * scanned - so the page ran further behind the busier the seat was. A read is
 * for a cold load, a reconnect and a seat swap: every slice has a frame.
 *
 * The subject is the SEAT, and the connection routes an update to it by the
 * slot the update carries, so a store's contents are this seat's and nothing
 * else's.
 *
 * **The subscription follows its HOLDERS, and the RECORD stays.** One page
 * opens the seat's subscription - its own answer is the whole record - and
 * every holder after it rides that one: a page drawing the seat, and the
 * conversation folding it. A page leaving gives its own hold back; the
 * subscription goes with the last of them, because a held subscription reads
 * as a showing seat to the server and showing spends the marks a seat earns.
 * The record stays either way, so a return draws what it holds, and while the
 * conversation holds the seat a return asks for nothing at all.
 */

import { writable, type Readable, type Writable } from 'svelte/store';

import { canHost } from '../browser/host';
import { cronNames } from '../chat/cron-names.svelte';
import { cannotPaint } from '../paint';
import { slotOf, subjectKey, type SessionUpdate, type Subject } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import type { Store } from '../stores';
import type { SessionSlot } from '../wire/types';
import { applyUpdate, REPLACES, variantOf } from './apply';
import { sessionFrom, type SessionRecord } from './wire';

/** What the session page has to draw from, and why it has nothing when it does not. */
export interface SessionRead {
  wire: SessionRecord | null;
  /** The server's own words for turning the subscription down, or `null`. */
  refused: string | null;
}

const NOTHING: SessionRead = { wire: null, refused: null };

/**
 * One seat, held by the client rather than by the page drawing it.
 *
 * The view is here rather than in the page so that coming back is free: a
 * store keeps the value it was last given, so the next subscriber is handed it
 * with nothing asked of the server.
 */
interface Seat {
  readonly view: Writable<SessionRead>;
  /**
   * The record as it is held, which is what an update is applied to.
   *
   * Held beside the store rather than read back out of it: the reducer needs
   * the value before this one, and a store is what a page reads, not a place
   * to compute from.
   */
  wire: SessionRecord | null;
  /**
   * The server's own words for turning the subscription down, or `null`.
   *
   * Beside the record because the store carries the two as one value, and a
   * frame's publish states no refusal of its own.
   */
  refused: string | null;
  /** The connection's own store for the subject, which a read is read from. */
  held: Store | null;
  /** How many subscriptions this seat opened, to give back as many when it is let go. */
  opened: number;
  /**
   * How many holders keep this seat's subscription open.
   *
   * **A subscription is opened once and held by whoever needs it** (Ved's
   * ruling, 2026-10-09: the conversation folds every frame of every seat):
   * the page drawing the seat, and the conversation keeping its frames. The
   * page leaving gives its own hold back, and the seat's subscription goes
   * with the LAST of them - so a page's return sends nothing while the
   * conversation still holds it, and a seat nobody holds is let go exactly as
   * it was before.
   */
  holders: number;
  /** What the seat is subscribed as: a later reader that can answer raises it. */
  answering: boolean;
  /**
   * Whether an ask of ours is in flight.
   *
   * **A wanted record is recorded at the moment it is wanted.** A read is the
   * whole server encode - hundreds of milliseconds on a busy seat - so one is
   * often in flight when something replaces the record, and that answer was
   * encoded from the state before it. Reading a flag at the answer instead
   * would take whatever landed as the new record, which is how the previous
   * occupant's conversation came to be drawn as this one's. So a replacement
   * wanted while an answer is in flight is asked for again rather than
   * assumed.
   *
   * Three things want a whole record: the first read, a reconnect (the
   * subscription is re-made, so its snapshot is the server's account of where
   * the seat is now), and a seat that spawned, connected or took a new
   * occupant.
   */
  asking: boolean;
  /**
   * The occupant whose frames this seat's record is built from: the epoch its
   * sequence belongs to.
   *
   * **Every frame of a seat's stream is numbered on that seat's own counter,
   * and an occupant change starts the count again at 1.** So the id is what
   * tells a frame from another epoch - which the client must wipe rather than
   * merge - from a gap in this one, which is repair it can ask for.
   */
  epoch: string | null;
  /**
   * The newest number this seat has applied.
   *
   * **One number where a count and a baseline used to be.** A frame whose
   * number is the successor applies; one ahead of it is a gap to ask for;
   * one behind it is a frame already drawn. And a whole-read answer carries
   * the number its own stream had reached, so an answer older than the
   * frames already applied is refused rather than walking the record back.
   */
  lastSeq: number;
  /**
   * The newest number this seat has SEEN, applied or held above a hole.
   *
   * **What tells an answer the stream has outrun.** A frame held above a hole
   * has not been applied, so it moves `lastSeq` only when its repair lands -
   * and an answer encoded before it would read as current against `lastSeq`
   * alone. This is the sighting, not the drawing.
   */
  seen: number;
  /**
   * `lastSeq` as it stood when the ask in flight went out.
   *
   * **What tells a stale answer from a counter that started over.** An answer
   * whose number is behind what the record holds is stale when frames arrived
   * after the ask - the answer was encoded before them, and asking again
   * converges on one encoded after. When NO frame arrived since the ask, the
   * same shape means the seat's stream restarted under it (a resume, a server
   * restart: the same occupant, the count from 1 again) - and that answer is
   * the truth about where the stream stands, so re-asking would never
   * converge on anything else.
   */
  askedAt: number;
  /**
   * How many answers in a row the frames have outrun, against [`OUTRUN_CAP`]:
   * past the cap the stale answer is dropped and the frames' own record
   * stands (#1472). Reset whenever an answer is taken, when one is dropped
   * past the cap, and on a new epoch.
   */
  outrun: number;
  /**
   * The first number the repair in flight is asking for, `null` when none is.
   */
  repairFrom: number | null;
  /**
   * Frames that arrived above a hole while its repair was on the way.
   *
   * **Held rather than drawn.** A repaired user row applied after the
   * assistant frames that streamed past it opens a turn of its own at the
   * foot, so the frames above a hole wait for the hole and draw in sequence
   * with it.
   */
  repairHeld: { seq: number; span: number; update: SessionUpdate }[];
  /** Hold the seat's subscription open, answering the release: see `hold`. */
  hold: () => () => void;
  /**
   * Open the seat's subscription: see `open`.
   *
   * **On the seat rather than a closure the caller could reach**, because the
   * role escalation happens outside this factory - and a bare `open()` out
   * there is the DOM global, which is a blank tab rather than a subscribe.
   */
  open: () => void;
}

/**
 * The seats this client has visited, by the connection they were subscribed on
 * and by the seat's own key.
 *
 * **Keyed by the key rather than by the subject**: a seat's subject is an
 * object, and the one the server answers with is a different object that says
 * the same thing, so a registry keyed on the object itself never matches and
 * every visit would subscribe afresh.
 *
 * **Keyed by the connection as well**, because a subscription lives on one
 * socket: a client that moved to another address holds a seat on each, and a
 * record held under the old one is not the new one's to draw. A seat goes with
 * the connection it was made on when that connection closes (`release`).
 */
const SEATS = new WeakMap<Connection, Map<string, Seat>>();

/**
 * How long a publish waits for a paint before it goes out anyway.
 *
 * `requestAnimationFrame` is the coalescer, not the guarantee: a scheduled
 * callback that never fires - whatever lost it, which is nothing this code
 * can know - must not leave the record dead, rows folded and never drawn. A
 * painting page clears the deadline long before it: a frame is 16ms at 60Hz
 * and this is a quarter second.
 *
 * **And the deadline is a STAMP, not only a timer, because a suspended page
 * loses both callbacks together** (#2132): the armed-at time is kept, the
 * next update past this flushes what the paint never did, and a return to
 * the page flushes with no update at all (`wake`).
 */
const PAINT_WATCHDOG_MS = 250;

/**
 * How many frames one seat holds above a hole before it draws them unordered.
 *
 * The hold is the repair's round trip, so it is a handful of frames in every
 * ordinary case; the cap is what keeps a repair whose answer never comes - or
 * a burst far larger than the hole - from growing without bound, and its
 * fallback is the arrival order the hold replaced.
 */
const REPAIR_HELD_CAP = 256;

/**
 * How many answers in a row the frames may outrun before the re-ask stops.
 *
 * A seat framed faster than a round trip outruns every answer, so without a
 * cap the re-ask re-arms on each trip, a full server encode apiece; a few
 * tries covers the ordinary race (a frame or two arriving mid-trip), and past
 * them the record the frames already hold IS the record - one more encode
 * will be outrun too (#1472, measured live 2026-10-15).
 */
const OUTRUN_CAP = 3;

/**
 * Watch one seat.
 *
 * **The seat's record is the client's; its subscription follows its holders.**
 * The first page to show a seat opens its subscription - that answer is what
 * fills its history - and every holder after that, a page or a conversation,
 * rides the one subscription and is handed what the seat holds. The
 * subscription goes with the last holder, and a return while any holder
 * remains asks for nothing: the record it held draws, and the frames never
 * stopped.
 *
 * **The subscribe happens with the first reader**, not at this call: a caller
 * that takes the store and never subscribes opens nothing of its own, and a
 * seat whose page is the only holder is subscribed by that page's own first
 * read. The second opener is the role: a page that can answer raises a seat
 * held as an observer, and that raise is a subscribe whether or not any page
 * has subscribed the store yet.
 *
 * **A seat the server refused gives its subscription back, and stays.** A
 * refusal is an answer rather than a subscription ("a refused subscribe
 * leaves nothing to hear"), so the record is cleared and the next visit opens
 * the seat afresh - while the seat itself is kept, because a conversation
 * holding it must go on holding whatever the next visit finds.
 */
export function watchSession(
  connection: Connection,
  slot: SessionSlot,
  /**
   * Whether this page can ANSWER the prompts it draws.
   *
   * **It is not a client's preference, and getting it wrong loses a turn
   * either way.** A client counted as answering with no surface to draw a
   * prompt on hangs the turn, because the core parks it on a reply that never
   * comes; a client that HAS the surface while subscribing as an observer
   * leans on whoever else can answer. So the caller states what it has: the
   * page says yes exactly when its composer - the thing with the dock in it -
   * is wired in.
   *
   * **The role belongs to the connection rather than to this seat**, so this
   * is a second statement of it rather than the first: the home's own
   * subscribe declares the same yes once for the whole client, and this one
   * covers a page that reaches a seat before that subscribe lands. Either
   * statement is enough, because the role only ever rises.
   *
   * **It is a safeguard rather than what keeps an ask alive.** Every update
   * reaches every subscriber whatever its role, and the role decides only
   * what the core may park on. What this covers is a serve with no answerer
   * anywhere, where an ask raised while no page could draw it would be
   * cancelled at birth.
   */
  answering: boolean,
): { subscribe: Readable<SessionRead>['subscribe']; hold: () => () => void } {
  const subject: Subject = { session: slot };
  const key = subjectKey(subject);
  const visited = seats(connection);
  let seat = visited.get(key);

  if (seat === undefined) {
    seat = createSeat(visited, connection, subject, answering);
    visited.set(key, seat);
  } else if (answering && !seat.answering) {
    // **The role only ever rises, and a page that can answer has to say so.**
    // A seat held before any page reached it has no subscription yet - the
    // conversation's hold keeps one alive, it does not open one - so this IS
    // its open, under the stronger role. A seat already subscribed as an
    // observer is re-asked under that role, which is one subscribe: what a
    // page mounting with a dock would have sent for itself.
    seat.answering = true;
    seat.open();
  }

  return { subscribe: seat.view.subscribe, hold: seat.hold };
}

/** The seats held on one connection, made on first use. */
function seats(connection: Connection): Map<string, Seat> {
  const held = SEATS.get(connection);
  if (held !== undefined) return held;
  const fresh = new Map<string, Seat>();
  SEATS.set(connection, fresh);
  return fresh;
}

/** One seat, opened: subscribed, listened to, and holding the record an update is applied to. */
function createSeat(
  visited: Map<string, Seat>,
  connection: Connection,
  subject: Subject,
  answering: boolean,
): Seat {
  const key = subjectKey(subject);
  // The seat this factory is for, for the asks that address one: every caller
  // builds it for a session subject.
  const held = typeof subject === 'object' ? subject.session : null;

  /**
   * A page is showing this seat: nothing is asked for it.
   *
   * **A read here would walk the record backwards.** It would re-derive the
   * record from the last snapshot the socket took, which is the record as it
   * was before every frame applied since; only a seat holding nothing reads,
   * and on a live connection that read is the subscription's own answer, in
   * flight already. The seat follows its frames from there.
   */
  function showing(): () => void {
    shown = true;
    seat.holders += 1;
    // A return: the page is back. **Nothing is asked for a seat whose
    // subscription is still open** - the conversation holds it for as long as
    // it folds the seat (Ved, 2026-10-09), and a re-ask would re-encode and
    // re-send a whole record this client already holds. `opened === 0` is the
    // one case a subscribe is owed: the last holder let the seat go, and this
    // is the first reader back. Its answer is the whole record, and the held
    // record draws until it lands.
    if (seat.opened === 0) open();
    if (seat.wire === null) read();
    return leaving;
  }

  /** This page has gone: its own hold on the seat goes with it. */
  function leaving(): void {
    shown = false;
    // No frame paints for a page that has gone, so a record still waiting for
    // one is written now: it is what a return draws.
    if (queued !== null) flush();
    // **A refused seat gives its subscription back, and stays.** A refusal is
    // an answer rather than a subscription ("a refused subscribe leaves
    // nothing to hear"), so nothing is left to draw and the next visit opens
    // the seat afresh - but the seat itself is kept, because a conversation
    // holding it must go on holding whatever the next visit finds. Forgetting
    // it here would leave that conversation's hold on a seat nothing owns, and
    // the next visit's page would give the subscription back with the
    // conversation still folding it.
    if (seat.held?.state().kind === 'refused') {
      if (seat.opened > 0) {
        for (let left = seat.opened; left > 0; left -= 1) connection.unsubscribe(subject);
        seat.opened = 0;
      }
      seat.held = null;
      publish(null, null);
      give_back();
      return;
    }
    give_back();
  }

  /**
   * One holder lets the seat go, and the subscription goes with the LAST of
   * them.
   *
   * **A held subscription reads as a SHOWING seat to the server**, and
   * showing spends every mark the seat earns - the failure mark and the
   * diamond both are cleared by it - so a seat nobody holds is given back.
   * What does not give it back is a page leaving a seat a conversation still
   * folds: the frames keep arriving for it (Ved, 2026-10-09), and the return
   * would otherwise pay a second encode of the record the client already
   * holds. The record and the held store stay either way, so a return draws
   * what it holds while a re-subscribe's answer - the whole record, as a
   * reconnect's is - lands.
   */
  function give_back(): void {
    if (seat.holders > 0) seat.holders -= 1;
    if (seat.holders > 0) return;
    if (seat.opened > 0) {
      for (let left = seat.opened; left > 0; left -= 1) connection.unsubscribe(subject);
      seat.opened = 0;
    }
  }

  /**
   * Keep the seat's subscription open until the returned release is called.
   *
   * The conversation's own hold, and the reason a left seat keeps folding:
   * the server sends a seat's frames to a subscriber of that seat, so a
   * subscription given back with the page would drop exactly the frames the
   * ruling keeps (Ved, 2026-10-09). Balanced with the page's own hold, so a
   * seat nobody holds is still let go.
   *
   * **It keeps a subscription alive; it does not open one.** The conversation
   * lives inside the page that draws the seat, so the page is what opens it -
   * as itself or as the role a page that can answer declares - and a hold
   * taken first opens nothing rather than subscribing a seat as an observer
   * the page would then have to re-ask under its own role.
   */
  function hold(): () => void {
    seat.holders += 1;
    return give_back;
  }

  const seat: Seat = {
    view: writable<SessionRead>(NOTHING, showing),
    wire: null,
    refused: null,
    held: null,
    opened: 0,
    holders: 0,
    answering,
    asking: false,
    epoch: null,
    lastSeq: 0,
    seen: 0,
    askedAt: 0,
    outrun: 0,
    repairFrom: null,
    repairHeld: [],
    hold,
    open,
  };

  /**
   * The frame a publish is waiting for, or `null`.
   *
   * **Applying an update is immediate and only the redraw waits.** A frame is
   * folded the moment it arrives - the fold is cheap, and the record it leaves
   * is what the next fold reads - while the store is written once per painted
   * frame, carrying the record as it stands then. So a stream arriving faster
   * than the display refreshes costs one redraw per painted frame rather than
   * one per update: nothing dropped, the order kept, the held record exact
   * throughout.
   */
  let queued: number | null = null;

  /**
   * The paint's own deadline, armed while `queued` is: see the chat's twin
   * (`chat/conversation.ts`) - a callback the browser drops must cost one
   * throttled publish, not every later row.
   */
  let watchdog: ReturnType<typeof setTimeout> | null = null;

  /**
   * When the waiting publish was armed, by the wall clock - `0` when none is.
   *
   * The timer is a promise from the page that a suspended page can break, so
   * the wait keeps its own record: `soon` and `wake` read this rather than
   * trusting either callback to come.
   *
   * **The clock's one hole is a backward wall-clock step**, which under-counts
   * the interval and withholds the stamp heal until the clock re-advances or a
   * return fires. Accepted because `performance.now` freezes while the machine
   * sleeps, so a monotonic stamp would read near zero after a lid close -
   * exactly where the heal is needed.
   */
  let armedAt = 0;

  /**
   * Whether a page is showing the seat, which is when a frame is waited for.
   *
   * A seat the client holds and nobody is drawing has no paint to wait for, so
   * it is written at once rather than scheduling a callback per frame for a
   * store no reader hears - and the client holds every seat it has visited.
   */
  let shown = false;

  /**
   * Write the record as it stands, with the refusal that goes with it, to the
   * store.
   *
   * A frame still waiting for its paint is dropped in the same breath: this
   * write states the record as it is NOW, and the paint would only state it
   * again.
   */
  function flush(): void {
    if (queued !== null) {
      cancelAnimationFrame(queued);
      queued = null;
    }
    if (watchdog !== null) {
      clearTimeout(watchdog);
      watchdog = null;
    }
    armedAt = 0;
    seat.view.set({ wire: seat.wire, refused: seat.refused });
  }

  /**
   * Publish an applied update at the next painted frame, or at once when no
   * page is drawing the seat.
   *
   * The record has already moved by the time this is called: this is only the
   * redraw.
   */
  function soon(): void {
    // **Hidden is asked FIRST, ahead of the in-flight guard.** A frame armed
    // on a page that has gone hidden never fires - and a display going off
    // mid-turn is how a page usually gets there, so one is armed at the
    // instant it happens. Waiting would swallow this fold and every fold
    // after it, so the write goes out now; `flush` cancels the frame and its
    // deadline in the same breath.
    if (cannotPaint()) {
      flush();
      return;
    }
    if (queued !== null) {
      // **The deadline is the stamp, not the timer.** A suspended page can
      // lose both callbacks, and the guard would then swallow this update and
      // every update after it: one this far past the arming flushes what the
      // callbacks never did.
      if (Date.now() - armedAt >= PAINT_WATCHDOG_MS) flush();
      return;
    }
    if (!shown) {
      flush();
      return;
    }
    queued = requestAnimationFrame(flush);
    armedAt = Date.now();
    watchdog = setTimeout(flush, PAINT_WATCHDOG_MS);
  }

  /**
   * The page can paint again: draw what a suspended paint never did.
   *
   * **Recovery here must not depend on a new update or a read**, which is
   * what the armed callbacks failed to give - the window coming back is
   * itself the moment. Only a pending publish is drawn: with none, the
   * drawn record is already the held one, because every write either
   * published or armed, and nothing else clears that.
   */
  function wake(): void {
    if (queued !== null) flush();
  }

  const visible = (): void => {
    if (!document.hidden) wake();
  };

  /**
   * The page's own returns, which ride the seat's lifetime.
   *
   * A suspended window loses the armed paint and can take its deadline with
   * it, so visibility returning, a bfcache restore and the window regaining
   * focus each draw the pending publish.
   *
   * **Registered for the seat and taken back with it.** A handler left on the
   * document closes over this seat and, through it, the connection it was made
   * on - and the shell replaces the connection when the address changes, so
   * one left behind pins that connection's whole store for the page's life
   * (#2136).
   */
  const pageReturns =
    typeof document !== 'undefined' && typeof window !== 'undefined'
      ? {
          listen(): void {
            document.addEventListener('visibilitychange', visible);
            window.addEventListener('pageshow', wake);
            window.addEventListener('focus', wake);
          },
          release(): void {
            document.removeEventListener('visibilitychange', visible);
            window.removeEventListener('pageshow', wake);
            window.removeEventListener('focus', wake);
          },
        }
      : null;

  /** Whether this seat's lifetime has ended: see `release`. */
  let released = false;

  /**
   * End the seat's lifetime.
   *
   * **A closed connection is gone for good** - nothing reopens one, and the
   * shell answers an address change with a new socket rather than with this
   * one - so the seat stops listening for the page and leaves the registry,
   * rather than waiting on a return that is never coming.
   */
  function release(): void {
    if (released) return;
    released = true;
    pageReturns?.release();
    visited.delete(key);
  }

  // A seat made on a connection that is already closed never hears the close
  // that would release it, so it takes no page listener at all.
  if (connection.status() !== 'closed') pageReturns?.listen();

  /**
   * Take a record - and the refusal that goes with it - and write it at once.
   *
   * A read's answer is already everything the seat reads as, and a refusal is
   * the whole of it too: neither is a frame in a stream, so neither waits for
   * a paint.
   */
  function publish(next: SessionRecord | null, refused: string | null): void {
    seat.wire = next;
    seat.refused = refused;
    flush();
  }

  /**
   * Take the record the seat is holding.
   *
   * **A read is the whole record and there is nothing else to take.** Every
   * slice a frame can carry has a handler, so an answer is not a refresh of a
   * few fields: it is the truth about the seat, and what a cold load, a
   * reconnect and a seat swap are owed.
   */
  function read(): void {
    if (seat.held === null) return;
    const state = seat.held.state();
    if (state.kind === 'refused') {
      publish(null, state.why);
      return;
    }
    const data = seat.held.snapshot();
    if (data === null) return;
    publish(withHeldTake(sessionFrom(data), seat.wire), null);
  }

  /**
   * Ask the server for the record again.
   *
   * Nothing is asked while one ask is in flight - a full encode apiece, and a
   * second would queue behind the first - so a replacement wanted now is
   * recorded and asked for when the answer lands.
   *
   * **A seat with no subscription asks nothing, because there is nothing to
   * ask on.** The ask is an unsubscribe-then-subscribe pair, so a seat whose
   * last holder let it go would have this re-attach it: the server reads it as
   * shown, spending the marks every reader would get, with no holder here
   * owning it. A held seat asks whether or not a page is drawing it - the
   * frames never stopped for it, and the return no longer re-reads, so this is
   * the only ask a seat away from the reader has.
   */
  function reread(): void {
    if (seat.asking || seat.opened === 0) return;
    seat.asking = true;
    // Where the stream stood when the ask left, so the answer can be read
    // against what has arrived since.
    seat.askedAt = seat.seen;
    connection.refresh(subject);
  }

  /**
   * Open the seat's subscription, which its first holder is owed.
   *
   * **One subscribe, and it opens with the first holder** - a page showing the
   * seat, or a page that can answer arriving at one held as an observer. Two
   * subscribes are two whole records encoded and sent to a client that keeps
   * one copy (6.5 MB twice on a large seat, measured 2026-10-09), and the
   * subscription goes with the LAST holder rather than with any one of them.
   * Its answer is the first whole record, and it is an ask this seat made: the
   * subscribe is what the server answers.
   */
  function open(): void {
    seat.held = connection.subscribe(subject, {
      answering: seat.answering,
      browser: canHost(),
    });
    seat.opened += 1;
    seat.asking = true;
    seat.askedAt = seat.seen;
  }

  /**
   * Listen to the seat: its frames, its refusals, and the socket's own life.
   *
   * Setup at the seat's creation rather than at its subscription: the seat
   * outlives any one subscription (a page's hold goes, a conversation's
   * stays), and a listener that came and went with it would drop the frames
   * that crossed in the gap.
   *
   * Nothing unregisters them: a seat lives on its connection, so its
   * listeners go with the connection when the client lets it go.
   */
  /**
   * Ask the seat for the frames between two numbers.
   *
   * **Silent, and never blocking.** The frames above the hole are held while
   * the answer is on its way, and the repaired stretch draws in sequence with
   * them - so a gap fills underneath the reader, in order, rather than the
   * reader watching a repaired row land below the frames that followed it. A
   * socket that is not open cannot ask, and nothing is kept for later: the
   * frames the reconnect's snapshot carries are the whole record, and the
   * numbers it stands at are where the reading resumes.
   */
  function askRange(from: number, to: number): boolean {
    if (to < from || held === null) return false;
    return connection.seatRange(held, seat.epoch, from, to);
  }

  /**
   * Draw the frames held while a repair was in flight, oldest first.
   *
   * A seal among them is a new occupant and takes its own path (a wipe and a
   * fresh read), which is why they go back through [`applyFrame`] rather than
   * straight to the record.
   */
  function flushRepaired(): void {
    const waiting = seat.repairHeld.splice(0).sort((a, b) => a.seq - b.seq);
    seat.repairFrom = null;
    for (const frame of waiting) applyFrame(frame, true);
  }

  /**
   * Apply one frame's update to the record and publish it.
   *
   * Nothing reaches a seat whose first read has not landed: the read answers
   * the frames to come as well, and drawing before it would double every row
   * it carries.
   */
  function draw(update: SessionUpdate): void {
    if (seat.wire === null) return;
    const next = applyUpdate(seat.wire, update);
    // An update this record has nothing to do with: publishing it would
    // redraw every reader of the page for no change at all.
    if (next === seat.wire) return;
    seat.wire = next;
    soon();
  }

  /**
   * One frame of the seat's stream, live or repaired: the sequence decides.
   *
   * **Every frame the workspace emits for a seat is numbered on that seat's
   * own counter**, and the number is what says what the frame IS to this
   * record: the successor of what it holds applies, one ahead of it is a gap
   * to ask for, and one behind it is a frame already drawn - a replay, a
   * repeat, or a reorder, none of which is a hole. A frame that carries no
   * number throws nothing (a delivery the server forged for this connection
   * is drawn as the turn it reaches the model as, and stands in no sequence).
   */
  function applyFrame(
    frame: {
      seq?: unknown;
      span?: unknown;
      update: SessionUpdate;
    },
    /**
     * Whether this frame is one this seat ASKED for.
     *
     * **A repaired frame is applied even where the number rule would skip
     * it.** The reader keeps reading what arrives while the repair is on its
     * way, so a frame ahead of the hole has usually landed by the time the
     * answer does - and the repaired frames come back numbered BELOW it,
     * which as live frames would read as already drawn. They are not: they
     * were never applied, and the sequence check exists to skip repeats, not
     * to drop the answer to an ask.
     */
    repaired = false,
  ): void {
    const [name, payload] = variantOf(frame.update);

    // The three variants that REPLACE the record rather than patching it: a
    // seat waking up, connecting, or taking a new occupant. What they carry
    // is not a record - the seat's own session facts, and no transcript (the
    // wire empties it; a seat's history arrives as a page, never as an update
    // payload) - so only a read answers them.
    if (name !== null && REPLACES.includes(name)) {
      // **A new occupant is a new epoch, and the count starts again at 1.**
      // Learned from the frame itself, so an answer still in flight from the
      // old stream is refused by its epoch rather than adopted.
      const occupant = payload['session_id'];
      if (typeof occupant === 'string' && occupant !== seat.epoch) {
        seat.epoch = occupant;
        seat.lastSeq = 0;
        seat.seen = 0;
        // Frames held under the old epoch are not the new one's to draw.
        seat.repairHeld.length = 0;
        seat.repairFrom = null;
        // A new epoch's outruns are its own; the old run's count would
        // shorten the next burst's grace (the take still terminates).
        seat.outrun = 0;
      }
      reread();
      return;
    }

    // A fired cron's schedule reaches no field of this record: the row's
    // prose carries the prompt, and the pairing lives on the frame alone.
    // Kept before the record's own early return, because this update is one
    // the record has nothing to say about.
    if (name === 'cron_prompt_appended') {
      cronNames.remember(payload['uuid'], payload['description']);
    }

    const seq = typeof frame.seq === 'number' ? frame.seq : null;
    const span = typeof frame.span === 'number' && frame.span > 0 ? frame.span : 1;
    if (seq !== null) {
      // The frame stands for `span` of the seat's numbers - a run of token
      // appends crosses as the one frame they draw as - and it carries the
      // OLDEST of them.
      const reach = seq + span - 1;
      seat.seen = Math.max(seat.seen, reach);
      if (!repaired) {
        // A frame behind the newest number this record holds is one it has
        // already drawn: a repeat, a replay or a reorder, and never a hole.
        if (reach <= seat.lastSeq) return;
        // **A hole holds everything above it until its repair lands.** Drawn
        // at once, a repaired user row would land below the assistant frames
        // that streamed past it and open a turn of its own at the foot; held,
        // the stretch draws in sequence - the hole, then what crossed it.
        if (seat.repairFrom !== null) {
          seat.repairHeld.push({ seq, span, update: frame.update });
          if (seat.repairHeld.length >= REPAIR_HELD_CAP) flushRepaired();
          return;
        }
        if (seq > seat.lastSeq + 1) {
          seat.repairFrom = seat.lastSeq + 1;
          seat.repairHeld.push({ seq, span, update: frame.update });
          if (!askRange(seat.repairFrom, seq - 1)) {
            // A socket that is not open cannot ask, so nothing is coming:
            // the held frames draw rather than waiting forever.
            flushRepaired();
          }
          return;
        }
      }
      // A repaired stretch can arrive either side of the frame that opened
      // the hole, so the record's number is the newest it has seen.
      seat.lastSeq = Math.max(seat.lastSeq, reach);
    }

    draw(frame.update);
  }

  /**
   * The epoch and the stream's reach, off the whole-read answer that carries
   * them.
   *
   * `seq` is the number the seat's next frame will bear, so the newest number
   * the answer covers is one less. Both are `null` from a server older than
   * the fields, which leaves the record's rule to the frames alone.
   */
  function reachOf(data: unknown): { epoch: string | null; seq: number | null } {
    const header = (data as { header?: { session_id?: unknown; seq?: unknown } } | null)?.header;
    return {
      epoch: typeof header?.session_id === 'string' ? header.session_id : null,
      seq: typeof header?.seq === 'number' ? header.seq : null,
    };
  }

  function watch(): void {
    connection.onMessage((message) => {
      if (message.kind === 'error') {
        // **An error names the operation it is about, never a subject**, and
        // this client holds every seat it has visited, so the subject is not
        // what tells this seat whether the error is its own. A refused
        // subscribe is given to the store it was attributed to, which is what
        // the socket does with the refusal, so this store reading `refused` IS
        // that answer.
        //
        // **Only that spends the ask in flight.** A blanket clear on any
        // subscribe refusal spends a seat's whole-record ask on a refusal
        // about another seat, and the answer it was waiting for is then
        // refused as outrun and asked again - the new occupant's record never
        // adopted, and the previous conversation standing until the next
        // answer. Anything else - a refused command, a page that could not be
        // read, a frame the server did not know - leaves the ask standing too,
        // and with it whatever whole record is still wanted.
        if (message.what === 'subscribe' && seat.held?.state().kind === 'refused') {
          seat.asking = false;
          // A refusal is the seat's own answer and a page draws why from it,
          // rather than keeping the record it read before the refusal.
          read();
          return;
        }
        // **A refused repair means read the seat again.** The numbers the
        // client holds came from a seat whose occupant has moved on: nothing
        // in this epoch can answer for them, and the whole record is what
        // tells it where the new one stands.
        if (message.what === 'seat_range' && message.seat !== undefined) {
          if (subjectKey({ session: message.seat }) !== key) return;
          // The frames held above the hole belong to the epoch the refusal
          // just ended; the whole read that follows answers for the new one.
          seat.repairHeld.length = 0;
          seat.repairFrom = null;
          reread();
        }
        return;
      }
      if (message.kind === 'snapshot') {
        // By KEY rather than by the subject: a seat's subject is an object, and
        // the one the server answers with is a different object saying the same
        // thing, so `===` never matches it.
        if (subjectKey(message.subject) !== key) return;
        seat.asking = false;
        const reach = reachOf(message.data);
        // **An occupant change is a wipe, not a merge.** A `/new` or a
        // `/resume` starts the seat's count again at 1, so a frame this record
        // holds numbers belongs to a stream that is gone - and the answer is
        // read afresh rather than reconciled with it.
        if (reach.epoch !== seat.epoch) {
          seat.epoch = reach.epoch;
          seat.lastSeq = reach.seq === null ? 0 : reach.seq - 1;
          seat.seen = seat.lastSeq;
          seat.outrun = 0;
          read();
          return;
        }
        // **An answer the frames have outrun is not taken - but only when the
        // frames got there first.** A number behind what the record holds is
        // stale when frames arrived after the ask: the answer was encoded
        // before them, and asking again converges on one encoded after.
        //
        // With no frame since the ask, the same shape is a stream that
        // RESTARTED under the client - a resume or a server restart, the same
        // occupant with the count from 1 again - and that answer is the truth
        // about where the stream stands. Refusing it would ask forever for an
        // answer that never comes, so it is taken: never a merge, and never a
        // loop.
        const moved = seat.seen !== seat.askedAt;
        if (reach.seq !== null && reach.seq - 1 < seat.lastSeq && moved && seat.wire !== null) {
          if (seat.outrun >= OUTRUN_CAP) {
            // **Past the cap the merged state is the record.** The frames
            // have carried the record past this answer, and taking the
            // answer would roll it back to its encode point and rewind the
            // count - numbers the ring may no longer hold are then never
            // re-fetched, rows lost (rule 25). So the answer is dropped
            // rather than taken, the stale body never reaches the store, and
            // the re-ask stops here (#1472).
            seat.outrun = 0;
            return;
          }
          seat.outrun += 1;
          reread();
          return;
        }
        // The answer is taken, so the run of outruns is over.
        seat.outrun = 0;
        if (reach.seq !== null) {
          seat.lastSeq = reach.seq - 1;
          seat.seen = Math.max(seat.seen, seat.lastSeq);
        }
        read();
        return;
      }
      // A repaired stretch, in answer to a range ask this seat made: the
      // frames fold through the SAME path a live frame does - the sequence
      // decides where each one lands - so the gap fills underneath the reader
      // rather than arriving as a second kind of drawing.
      if (message.kind === 'seat_frames') {
        if (subjectKey({ session: message.slot }) !== key) return;
        for (const frame of message.frames) applyFrame(frame, true);
        // The hole is filled, so what crossed it draws now - in sequence,
        // which is the order the hold was for.
        flushRepaired();
        return;
      }
      if (message.kind !== 'update') return;
      // **The connection is not this seat's alone.** The shell holds the home
      // for its whole life, and the server filters per CONNECTION over the
      // union of the subjects watched, so every other seat's frames arrive
      // here too. This is the server's own covering rule for a session - a
      // seat hears an update when the update's slot is that seat - and
      // without it a busy fleet would write another seat's frames into this
      // page's record.
      const at = slotOf(message.update);
      if (at === null || subjectKey({ session: at }) !== key) return;
      applyFrame(message);
    });
    // A drop takes any read in flight with it, and the reconnect answers with a
    // snapshot of its own - so the pacing must not stay stuck waiting on an
    // answer that died. The seat keeps this listener, so a drop while nobody is
    // showing the seat still leaves the reconnect's snapshot answered as the
    // whole record it is.
    connection.onStatus((next: ConnectionStatus) => {
      if (next === 'closed') {
        // **The seat's lifetime ends where its connection's does.** A drop
        // leaves the same connection reconnecting and this seat drawing what
        // it holds, so only a close - the shell replacing the socket on an
        // address change - releases it.
        release();
        return;
      }
      if (next !== 'open') {
        // The ask in flight went with the drop, and the record this seat holds
        // is from before it: the answer the reconnect brings is the whole
        // record, so one is wanted.
        seat.asking = false;
        // The take goes with the socket: the core drops a take whose reader
        // went away, so the fold's own claim dies here - left standing, the
        // record draws a recording that is over (#1880).
        if (seat.wire !== null && seat.wire.composer.take !== null) {
          seat.wire = { ...seat.wire, composer: { ...seat.wire.composer, take: null } };
          soon();
        }
      }
    });
  }

  watch();
  return seat;
}

/**
 * A whole-record answer carries no take, and the fold's own must not go with
 * it: the row is drawn off the record, so a replace that dropped the take
 * this connection's own updates had drawn emptied the screen while the
 * recording stayed live (#1880). The take is this connection's, and the
 * socket leaving `open` is what ends that claim.
 *
 * The take is carried and its sibling NOTICE is not: a take is live state a
 * replace must not touch, while a notice is a moment - and only a take is
 * owed back to a reader who was away - so a refusal line landing just before
 * a snapshot is the one thing this can wipe.
 */
function withHeldTake(next: SessionRecord, previous: SessionRecord | null): SessionRecord {
  const held = previous?.composer.take ?? null;
  if (held === null) return next;
  return { ...next, composer: { ...next.composer, take: held } };
}
