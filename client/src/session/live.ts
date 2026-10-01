/**
 * The session over a live connection: the record it is answered with, and the
 * update it applies to it.
 *
 * **The update is the news, and it is already everything a page needs.** The
 * terminal applies each variant to the state it holds, which is why it keeps
 * up with a busy seat that this page used to fall behind: answering every
 * update with a read is a full encode on the server - the transcript re-folded
 * into turns, the process tree walked, the working tree scanned - so the page
 * ran further behind the busier the seat was. A read is for a cold load, for a
 * reconnect, and for the slices no update carries.
 *
 * The subject is the SEAT, and the connection routes an update to it by the
 * slot the update carries, so a store's contents are this seat's and nothing
 * else's.
 *
 * **The subscription belongs to the client rather than to the page showing
 * it.** A seat the client has visited stays subscribed: a page leaving gives
 * nothing back, the frames arriving while nothing is drawing the seat are
 * applied to it, and coming back draws what is held rather than reading the
 * whole seat again. That read is the burst this exists to delete, and holding
 * every seat's state is what the terminal has done for the life of its
 * process for the same reason.
 */

import { writable, type Readable, type Writable } from 'svelte/store';

import { slotOf, subjectKey, type Subject } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import type { Store } from '../stores';
import type { SessionSlot } from '../wire/types';
import { applyUpdate, REPLACES, UNFED, variantOf } from './apply';
import { sessionFrom, type SessionRecord } from './wire';

/**
 * How often the slices no update carries are read.
 *
 * A read is a whole encode, and the nine slices it is asked for are the
 * slowest-moving part of the record - a git scan and a process walk do not
 * change between one frame and the next - so this is the coarsest read that
 * keeps a page honest rather than the cheapest that keeps it moving.
 */
export const POLL_MS = 5_000;

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
  /** The connection's own store for the subject, which a read is read from. */
  held: Store | null;
  /** How many subscriptions this seat opened, to give back as many when it is let go. */
  opened: number;
  /** What the seat is subscribed as: a later reader that can answer raises it. */
  answering: boolean;
  /**
   * What a whole record is wanted for, and whether one is wanted at all.
   *
   * **An ask is recorded with what it was issued for, at the moment it went
   * out.** A read is the whole server encode - hundreds of milliseconds on a
   * busy seat - so one is often in flight when something replaces the record,
   * and that answer was encoded from the state before it. Reading a flag at
   * the answer instead would take whatever landed as the new record, which is
   * how the previous occupant's conversation came to be drawn as this one's.
   * So the mode travels WITH the ask, and a replacement wanted while an answer
   * is in flight is asked for again rather than assumed.
   *
   * Three things want a whole record: the first read, a reconnect (the
   * subscription is re-made, so its snapshot is the server's account of where
   * the seat is now), and a seat that spawned, connected or took a new
   * occupant. Every other answer is a poll's, and a poll moves only the slices
   * no update feeds.
   */
  asking: 'replace' | 'merge' | null;
  replaceWanted: boolean;
  /** A poll's timer, armed while a page is showing the seat. */
  poll: ReturnType<typeof setInterval> | null;
  stopMessages: (() => void) | null;
  stopStatus: (() => void) | null;
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
 * record held under the old one is not the new one's to draw.
 */
const SEATS = new WeakMap<Connection, Map<string, Seat>>();

/**
 * Watch one seat.
 *
 * **The seat is the client's; a page only reads it.** The first page to show a
 * seat subscribes it - the subscription's own answer is what fills its history
 * - and every page after that is handed what the seat holds. Leaving gives
 * nothing back, so a return subscribes nothing and reads nothing.
 *
 * **A seat the server refused is the one exception**, and it is let go with
 * its last reader: a refusal is an answer rather than a subscription ("a
 * refused subscribe leaves nothing to hear"), so holding one holds nothing,
 * and a seat that starts later would never be reached again.
 */
export function watchSession(
  connection: Connection,
  slot: SessionSlot,
  /**
   * Whether this page can ANSWER the prompts it draws.
   *
   * **It is not a client's preference, and getting it wrong loses a turn
   * either way.** A client counted as answering that cannot display a prompt
   * hangs the turn, because the core parks it on a reply that never comes; a
   * client drawing a dock while subscribed as an observer has every prompt it
   * shows cancelled. So the caller states what it has: the page says yes
   * exactly when its composer - the thing with the dock in it - is wired in.
   */
  answering: boolean,
): Readable<SessionRead> {
  const subject: Subject = { session: slot };
  const key = subjectKey(subject);
  const visited = seats(connection);
  let seat = visited.get(key);

  if (seat === undefined) {
    seat = createSeat(connection, subject, answering, () => visited.delete(key));
    visited.set(key, seat);
  } else if (answering && !seat.answering) {
    // **The role only ever rises, and a page that can answer has to say so.**
    // A seat already subscribed as an observer is re-asked under the stronger
    // role, which is one subscribe - what a page mounting with a dock would
    // have sent for itself.
    seat.answering = true;
    seat.opened += 1;
    connection.subscribe(subject, { answering: true });
  }

  return { subscribe: seat.view.subscribe };
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
  connection: Connection,
  subject: Subject,
  answering: boolean,
  forget: () => void,
): Seat {
  const key = subjectKey(subject);

  /**
   * A page is showing this seat: the poll runs, and nothing else is asked.
   *
   * **A read here would walk the record backwards.** It would re-derive the
   * record from the last snapshot the socket took, which is the record as it
   * was before every frame applied since; only a seat holding nothing reads,
   * and on a live connection that read is the subscription's own answer, in
   * flight already.
   */
  function showing(): () => void {
    if (seat.wire === null) read(true);
    seat.poll = setInterval(() => {
      reread();
    }, POLL_MS);
    return leaving;
  }

  /**
   * The last reader has gone: the poll stops with it, because it is a whole
   * server encode per tick and nothing is drawing what it keeps honest.
   */
  function leaving(): void {
    if (seat.poll !== null) clearInterval(seat.poll);
    seat.poll = null;
    if (seat.held?.state().kind === 'refused') release();
  }

  const seat: Seat = {
    view: writable<SessionRead>(NOTHING, showing),
    wire: null,
    held: null,
    opened: 0,
    answering,
    asking: null,
    replaceWanted: true,
    poll: null,
    stopMessages: null,
    stopStatus: null,
  };

  function publish(next: SessionRecord | null, refused: string | null): void {
    seat.wire = next;
    seat.view.set({ wire: next, refused });
  }

  function read(replace: boolean): void {
    if (seat.held === null) return;
    const state = seat.held.state();
    if (state.kind === 'refused') {
      publish(null, state.why);
      return;
    }
    const data = seat.held.snapshot();
    if (data === null) return;
    const fresh = sessionFrom(data);
    publish(replace || seat.wire === null ? fresh : merged(seat.wire, fresh), null);
  }

  /**
   * Ask the server for the record again.
   *
   * Nothing is asked while one ask is in flight - a full encode apiece, and a
   * second would queue behind the first - so a replacement wanted now is
   * recorded and asked for when the answer lands.
   */
  function reread(): void {
    if (seat.asking !== null) return;
    seat.asking = seat.replaceWanted ? 'replace' : 'merge';
    seat.replaceWanted = false;
    connection.refresh(subject);
  }

  function watch(): void {
    seat.held = connection.subscribe(subject, { answering: seat.answering });
    seat.opened += 1;
    // The subscription's own answer is the first whole record, and it is an
    // ask this page made: the subscribe is what the server answers.
    seat.asking = 'replace';
    seat.replaceWanted = false;
    seat.stopMessages = connection.onMessage((message) => {
      if (message.kind === 'error') {
        // **An error names the operation it is about, never a subject**, so it
        // does not answer the ask in flight. The one operation that can mean
        // the ask is never coming back is a refused subscribe; anything else -
        // a refused command, a page that could not be read, a frame the server
        // did not know - leaves it standing, and with it whatever whole record
        // is still wanted.
        //
        // Treating a refusal as this seat's rests on one seat per connection:
        // the socket hangs a refusal on the oldest ask the connection has
        // outstanding, and only a session subject can be refused at all. A
        // second seat on one connection would need the subject on the error to
        // tell the two apart.
        if (message.what === 'subscribe') {
          seat.asking = null;
          // A refusal is the seat's own answer and a page draws why from it;
          // an error about anything else leaves the record as it is rather
          // than republishing it unchanged.
          if (seat.held?.state().kind === 'refused') read(false);
        }
        return;
      }
      if (message.kind === 'snapshot') {
        // By KEY rather than by the subject: a seat's subject is an object, and
        // the one the server answers with is a different object saying the same
        // thing, so `===` never matches it.
        if (subjectKey(message.subject) !== key) return;
        // What this answer is: the ask records what it was issued for, and an
        // answer to no ask of ours - the subscription's own, or a reconnect's -
        // is whatever the seat needs next. A whole record wanted while an ask
        // was out cannot be answered by that ask's answer, whatever it was.
        const mode = seat.asking ?? (seat.replaceWanted ? 'replace' : 'merge');
        const stale = seat.asking !== null && seat.replaceWanted;
        seat.asking = null;
        read(mode === 'replace' && !stale);
        if (stale) reread();
        else seat.replaceWanted = false;
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

      // The three variants that REPLACE the record rather than patching it: a
      // seat waking up, connecting, or taking a new occupant. What they carry
      // is not a record - the folded transcript, the process walk, the working
      // tree - so only a read answers them.
      const [name] = variantOf(message.update);
      if (name !== null && REPLACES.includes(name)) {
        seat.replaceWanted = true;
        reread();
        return;
      }

      // Nothing to apply to yet: the first read has not landed, and the frames
      // to come are answered by it.
      if (seat.wire === null) return;
      const next = applyUpdate(seat.wire, message.update);
      // An update this record has nothing to do with: publishing it would
      // redraw every reader of the page for no change at all.
      if (next === seat.wire) return;
      publish(next, null);
    });
    // A drop takes any read in flight with it, and the reconnect answers with a
    // snapshot of its own - so the pacing must not stay stuck waiting on an
    // answer that died. The seat keeps this listener, so a drop while nobody is
    // showing the seat still leaves the reconnect's snapshot answered as the
    // whole record it is.
    seat.stopStatus = connection.onStatus((next: ConnectionStatus) => {
      if (next !== 'open') {
        // The ask in flight went with the drop, and the record this seat holds
        // is from before it: the answer the reconnect brings is a whole one
        // rather than a poll's.
        seat.asking = null;
        seat.replaceWanted = true;
      }
    });
  }

  /**
   * Let the seat go, with everything it holds.
   *
   * The one path a seat is not kept through, and the reason is that there is
   * nothing behind it to keep: the server watches nothing for a seat it
   * refused.
   */
  function release(): void {
    seat.stopMessages?.();
    seat.stopStatus?.();
    seat.stopMessages = null;
    seat.stopStatus = null;
    if (seat.poll !== null) clearInterval(seat.poll);
    seat.poll = null;
    for (let left = seat.opened; left > 0; left -= 1) connection.unsubscribe(subject);
    seat.opened = 0;
    seat.held = null;
    seat.wire = null;
    forget();
  }

  watch();
  return seat;
}

/**
 * A poll's answer, keeping only the slices no update carries.
 *
 * **The conversation is the one to watch here.** A poll is asked for while
 * frames are arriving, and its answer was encoded after some of them and
 * before others - so taking its whole record would drop the frames that
 * landed in between, and the page would walk backwards on a busy seat.
 */
function merged(held: SessionRecord, fresh: SessionRecord): SessionRecord {
  const out = { ...held };
  for (const field of UNFED) {
    Object.assign(out, { [field]: fresh[field] });
  }
  return out;
}
