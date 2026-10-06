/**
 * The session over a live connection: the record it is answered with, and the
 * update it applies to it.
 *
 * **The update is the news, and it is already everything a page needs.** The
 * terminal applies each variant to the state it holds, which is why it keeps
 * up with a busy seat that this page used to fall behind: answering every
 * update with a read is a full encode on the server - the transcript re-folded
 * into turns, the process tree walked, the working tree scanned - so the page
 * ran further behind the busier the seat was. A read is for a cold load, a
 * reconnect and a seat swap: every slice has a frame.
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

import { canHost } from '../browser/host';
import { cronNames } from '../chat/cron-names.svelte';
import { slotOf, subjectKey, type Subject } from '../protocol';
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
  replaceWanted: boolean;
  /**
   * How many of this seat's frames have been applied to the record.
   *
   * **An answer older than a frame cannot be adopted whole**, and this is how
   * that is known: an ask records the count it was issued at, and an answer
   * landing after the count moved is refused and asked for again rather than
   * replacing a record the frames have already carried past it.
   *
   * A turn's own end is the case that keeps it: `turn_complete` settles the
   * turn as a frame, and an answer from before it puts a running turn back -
   * so the box refuses a send the turn has already taken.
   */
  frames: number;
  /** The frame count the ask in flight was issued at. */
  askedAt: number;
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
 * **The subscribe happens at this call**, not when the returned store gets its
 * first subscriber. Taking the store and never subscribing to it therefore
 * leaves a subscription standing, and the only thing that releases a seat is a
 * refusal.
 *
 * **A seat the server refused is that exception**, and it is let go with its
 * last reader: a refusal is an answer rather than a subscription ("a refused
 * subscribe leaves nothing to hear"), so holding one holds nothing, and a seat
 * that starts later would never be reached again.
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
    connection.subscribe(subject, { answering: true, browser: canHost() });
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
    if (seat.wire === null) read();
    return leaving;
  }

  /** The last reader has gone. */
  function leaving(): void {
    shown = false;
    // No frame paints for a page that has gone, so a record still waiting for
    // one is written now: it is what a return draws.
    if (queued !== null) flush();
    if (seat.held?.state().kind === 'refused') release();
  }

  const seat: Seat = {
    view: writable<SessionRead>(NOTHING, showing),
    wire: null,
    refused: null,
    held: null,
    opened: 0,
    answering,
    asking: false,
    replaceWanted: true,
    frames: 0,
    askedAt: 0,
    stopMessages: null,
    stopStatus: null,
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
    if (queued !== null) return;
    if (!shown) {
      flush();
      return;
    }
    queued = requestAnimationFrame(flush);
  }

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
    publish(sessionFrom(data), null);
  }

  /**
   * Ask the server for the record again.
   *
   * Nothing is asked while one ask is in flight - a full encode apiece, and a
   * second would queue behind the first - so a replacement wanted now is
   * recorded and asked for when the answer lands.
   */
  function reread(): void {
    if (seat.asking) return;
    seat.asking = true;
    seat.replaceWanted = false;
    seat.askedAt = seat.frames;
    connection.refresh(subject);
  }

  function watch(): void {
    seat.held = connection.subscribe(subject, {
      answering: seat.answering,
      browser: canHost(),
    });
    seat.opened += 1;
    // The subscription's own answer is the first whole record, and it is an
    // ask this page made: the subscribe is what the server answers.
    seat.asking = true;
    seat.replaceWanted = false;
    seat.askedAt = seat.frames;
    seat.stopMessages = connection.onMessage((message) => {
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
        }
        return;
      }
      if (message.kind === 'snapshot') {
        // By KEY rather than by the subject: a seat's subject is an object, and
        // the one the server answers with is a different object saying the same
        // thing, so `===` never matches it.
        if (subjectKey(message.subject) !== key) return;
        // What this answer is: the ask records whether one was ours, and an
        // answer to no ask - the subscription's own, or a reconnect's - is the
        // whole record this seat needs.
        const ours = seat.asking;
        // A whole record wanted while an ask was out cannot be answered by that
        // ask's answer, whatever it was.
        const stale = ours && seat.replaceWanted;
        // And the other way an answer is older than what is held: a frame
        // landed after the ask went out, so the record has been carried past
        // where this answer was encoded.
        const moved = ours && seat.frames !== seat.askedAt;
        seat.asking = false;
        // **An answer the frames have outrun is not taken.** A read is the
        // whole record now, so applying one encoded before a frame that has
        // already landed would walk the record back: the want is set and asked
        // again rather than spent, and the fresh answer lands on the first
        // clean window.
        if ((stale || moved) && seat.wire !== null) {
          seat.replaceWanted = true;
          reread();
          return;
        }
        seat.replaceWanted = false;
        read();
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
      // A frame for this seat is the world moving under any answer in flight,
      // counted before the ask such a frame may itself issue so that the ask's
      // own baseline includes it.
      seat.frames += 1;

      // The three variants that REPLACE the record rather than patching it: a
      // seat waking up, connecting, or taking a new occupant. What they carry
      // is not a record - the seat's own session facts and the folded
      // transcript - so only a read answers them.
      const [name, payload] = variantOf(message.update);
      if (name !== null && REPLACES.includes(name)) {
        seat.replaceWanted = true;
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

      // Nothing to apply to yet: the first read has not landed, and the frames
      // to come are answered by it.
      if (seat.wire === null) return;
      const next = applyUpdate(seat.wire, message.update);
      // An update this record has nothing to do with: publishing it would
      // redraw every reader of the page for no change at all.
      if (next === seat.wire) return;
      seat.wire = next;
      soon();
    });
    // A drop takes any read in flight with it, and the reconnect answers with a
    // snapshot of its own - so the pacing must not stay stuck waiting on an
    // answer that died. The seat keeps this listener, so a drop while nobody is
    // showing the seat still leaves the reconnect's snapshot answered as the
    // whole record it is.
    seat.stopStatus = connection.onStatus((next: ConnectionStatus) => {
      if (next !== 'open') {
        // The ask in flight went with the drop, and the record this seat holds
        // is from before it: the answer the reconnect brings is the whole
        // record, so one is wanted.
        seat.asking = false;
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
    for (let left = seat.opened; left > 0; left -= 1) connection.unsubscribe(subject);
    seat.opened = 0;
    seat.held = null;
    seat.wire = null;
    forget();
  }

  watch();
  return seat;
}
