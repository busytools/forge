/**
 * One ordered stream per seat, for EVERY consumer of a connection.
 *
 * **The rule cannot live in one consumer.** A connection's frames are read by
 * the seat store (`session/live`) and by the chat fold (`chat/conversation`)
 * as two independent listeners over one message pump - and on the desktop by
 * the Rust shim's own two readers. A hole healed inside one of them leaves
 * the other applying the frames above the hole at once, so a repaired user
 * row still lands under the assistant frames that crossed it and opens a turn
 * of its own (measured live, 3 of 3). Ordering goes here: between the wire
 * and every reader, once.
 *
 * The numbers are the seat's own (see the seat-snapshot-and-sequence design):
 * each update frame carries the oldest `seq` it stands for and how many
 * numbers `span` it covers. Numbers nobody has seen are MISSING ranges; the
 * frames above them wait, a `seat_range` ask goes out per range, and once the
 * ranges are filled everything is emitted in strict sequence - repaired
 * frames back inside `seat_frames` messages (that message is the call path
 * that strips a repeat and keeps a repair from arming the follow), waiting
 * frames as the updates they were. A second hole inside the held run opens
 * its own ask: a release that stepped past an unfilled range would swallow
 * whatever fell in it (seen once live, and pinned by a unit here).
 *
 * What this layer deliberately does NOT do: drop anything. A frame behind the
 * newest number still reaches the readers (the store skips it as drawn; the
 * chat strips a repeat by row identity), and a frame carrying no number - a
 * delivery the server forged, or anything from a server older than the field -
 * passes through untouched.
 */

import { slotOf, subjectKey, type ServerMessage, type SessionUpdate } from '../protocol';
import type { SessionSlot } from '../wire/types';

/** How many frames one seat waits with before it releases them unordered. */
const HELD_CAP = 256;

/**
 * How long a hold waits for its answers before releasing the frames.
 *
 * **A hold with no answer coming must not freeze the seat.** A socket that
 * dropped mid-ask, or a range no window could fill, would otherwise wait
 * forever; the release is the fallback the readers' own rules (the store's
 * second-layer hold, the chat's row identity) already know how to take.
 */
const HELD_DEADLINE_MS = 5000;

/** One numbered frame: an update or a repaired frame, with what it covers. */
interface Frame {
  seq: number;
  span: number;
  update: SessionUpdate;
}

/** A range of numbers nobody has delivered or repaired yet. */
interface Range {
  from: number;
  to: number;
}

interface StreamState {
  /** The occupant the numbers belong to, off the seal or the answer. */
  epoch: string | null;
  /**
   * Whether this connection follows the seat: a snapshot has said where its
   * stream stands.
   *
   * **Frames arrive for seats nothing here follows.** The home watch carries
   * fleet news per seat - a turn finishing, an occupant changing - numbered
   * on that seat's own stream, so holding or asking for a seat this
   * connection never opened demands a history nobody asked for: an awake
   * seat refuses the ask outright (`Some(id) != None`), which reappears on
   * the next fleet frame and loops (measured live, 2026-10-15: the client
   * asked `1..6956` with no occupant every ~30s), and a sleeping one answers
   * with its whole transcript, megabytes nobody opened. Unanchored, the
   * frames pass through in arrival order.
   */
  anchored: boolean;
  /** The next number the run owes, in strict sequence. */
  expecting: number;
  /**
   * The highest number the run has SEEN, waiting or delivered.
   *
   * **What says where the holes are.** A frame's own numbers are never
   * missing, however long it waits - only the stretch between what was last
   * seen and a frame that jumps ahead is.
   */
  seenThrough: number;
  /** Ranges seen as holes and not yet filled, sorted and merged. */
  missing: Range[];
  /** The ranges an ask has already gone out for. */
  asked: Range[];
  /** Update frames seen but not yet deliverable, in arrival order. */
  waiting: Frame[];
  /** Repaired frames received but not yet delivered, in arrival order. */
  repairs: Frame[];
  /** The slot and epoch of the `seat_frames` answers the repairs came in. */
  origin: { slot: SessionSlot; epoch: string | null } | null;
  deadline: ReturnType<typeof setTimeout> | null;
}

/** What a gate needs from the connection it orders. */
export interface GateDeps {
  /** Send a range ask; `false` when the socket is not open. */
  ask: (slot: SessionSlot, epoch: string | null, from: number, to: number) => boolean;
  /** Hand messages to every consumer, in order. */
  emit: (messages: ServerMessage[]) => void;
  /**
   * Whether the connection still holds this seat.
   *
   * **A snapshot can land after its own unsubscribe.** A page that opens a
   * seat and leaves inside one round trip releases it, and the subscribe's
   * answer is already on the wire - anchoring on it would leave a seat this
   * connection no longer follows sequenced for the life of the connection,
   * reopening the whole-history ask `releaseSeat` exists to close (found in
   * review, 2026-10-15). The connection is the only one that knows.
   */
  follows: (slot: SessionSlot) => boolean;
}

function stateOf(streams: Map<string, StreamState>, key: string): StreamState {
  let state = streams.get(key);
  if (state === undefined) {
    state = {
      epoch: null,
      anchored: false,
      expecting: 1,
      seenThrough: 0,
      missing: [],
      asked: [],
      waiting: [],
      repairs: [],
      origin: null,
      deadline: null,
    };
    streams.set(key, state);
  }
  return state;
}

/** The variants that replace a seat's record: an occupant change or a wake. */
function replacer(update: SessionUpdate): Record<string, unknown> | null {
  const held = update as Record<string, unknown>;
  for (const name of ['connected', 'session_replaced']) {
    const payload = held[name];
    if (payload !== null && typeof payload === 'object') {
      return payload as Record<string, unknown>;
    }
  }
  return null;
}

/** Merge `range` into a sorted, merged list. */
function cover(ranges: Range[], range: Range): void {
  let at = 0;
  let head = ranges[at];
  while (head !== undefined && head.to < range.from - 1) {
    at += 1;
    head = ranges[at];
  }
  let merged = { ...range };
  while (head !== undefined && head.from <= merged.to + 1) {
    merged = { from: Math.min(merged.from, head.from), to: Math.max(merged.to, head.to) };
    ranges.splice(at, 1);
    head = ranges[at];
  }
  ranges.splice(at, 0, merged);
}

/** Take `from..=to` out of a sorted, merged list: those numbers are filled. */
function filled(ranges: Range[], from: number, to: number): void {
  const kept: Range[] = [];
  for (const range of ranges) {
    if (range.to < from || range.from > to) {
      kept.push(range);
      continue;
    }
    if (range.from < from) kept.push({ from: range.from, to: from - 1 });
    if (range.to > to) kept.push({ from: to + 1, to: range.to });
  }
  ranges.splice(0, ranges.length, ...kept);
}

/** Whether `ranges` covers `range` entirely. */
function covers(ranges: Range[], range: Range): boolean {
  return ranges.some((held) => held.from <= range.from && held.to >= range.to);
}

export class StreamGate {
  private readonly streams = new Map<string, StreamState>();

  constructor(private readonly deps: GateDeps) {}

  /**
   * One inbound message; whatever should reach the consumers now, in order.
   *
   * A message that opens a hold returns `[]` and reaches the consumers later,
   * through `emit`, when its repairs land (or the deadline fires).
   */
  inbound(message: ServerMessage): ServerMessage[] {
    switch (message.kind) {
      case 'snapshot': {
        const subject = message.subject as { session?: SessionSlot };
        if (subject?.session === undefined) return [message];
        // A snapshot for a seat this connection has let go is the tail of a
        // subscription that is already over: it passes through and anchors
        // nothing.
        if (!this.deps.follows(subject.session)) return [message];
        const key = subjectKey({ session: subject.session });
        const state = stateOf(this.streams, key);
        // **The snapshot is the anchor.** It is the one message that says
        // where this seat's stream stands for THIS connection, so a seat is
        // sequenced only once one has landed for it.
        state.anchored = true;
        const header = (message.data as { header?: { session_id?: unknown; seq?: unknown } } | null)
          ?.header;
        const occupant = typeof header?.session_id === 'string' ? header.session_id : null;
        if (occupant !== null && occupant !== state.epoch) {
          // A new occupant counts from 1 again, hold and all.
          this.clear(state);
          state.epoch = occupant;
          state.expecting = 1;
          state.seenThrough = 0;
        }
        if (typeof header?.seq === 'number' && this.settled(state)) {
          // **The answer is the authority on where the stream stands**, the
          // same rule the store reads it by: a restart under the same
          // occupant counts from 1 again, and an answer that says so must
          // lower what this expects - a monotonic maximum would leave the
          // restarted run's frames reading as already drawn. A hold in
          // flight is where the answer is NOT: its ranges decide.
          state.expecting = header.seq;
          state.seenThrough = header.seq - 1;
        }
        return [message];
      }
      case 'update': {
        const slot = slotOf(message.update);
        if (slot === null) return [message];
        const key = subjectKey({ session: slot });
        const state = stateOf(this.streams, key);
        // A seat this connection has not been told the standing of: the
        // frame is news riding a coarser watch, and ordering it would ask
        // for a history nobody opened. Arrival order is what it gets.
        if (!state.anchored) return [message];

        const seal = replacer(message.update);
        if (seal !== null) {
          // **Every seal restarts the count, not only a new occupant.** The
          // server resets its counter for every `Connected`/`SessionReplaced`
          // whatever the id says (a restart under the same occupant counts
          // from 1 again), so this expects 1 after one - holding only on an
          // id change would read the restarted run's frames as already drawn
          // and never hold a hole in it.
          const occupant = seal['session_id'];
          this.clear(state);
          state.expecting = 1;
          state.seenThrough = 0;
          if (typeof occupant === 'string') state.epoch = occupant;
          // The seal is the wipe signal itself: it goes out at once.
          return [message];
        }

        const seq = typeof message.seq === 'number' ? message.seq : null;
        if (seq === null) return [message];
        const span = typeof message.span === 'number' && message.span > 0 ? message.span : 1;
        const reach = seq + span - 1;

        if (reach < state.expecting) {
          // Already delivered as far as this run is concerned: a repeat, a
          // replay or a reorder - the readers decide what to do with it.
          return [message];
        }
        state.waiting.push({ seq, span, update: message.update });
        if (seq > state.seenThrough + 1) {
          // A hole opens here - and one that opens while an earlier hole is
          // still unfilled gets its own ask, because a release stepping past
          // it would swallow whatever fell in it.
          this.open(state, { from: state.seenThrough + 1, to: seq - 1 }, slot);
        }
        state.seenThrough = Math.max(state.seenThrough, reach);
        // **Its own numbers are seen, so they are not missing** - a frame
        // arriving out of order can fill a range without any repair, and a
        // missing-set left holding them would block the next answer's
        // adoption and arm a deadline for nothing.
        filled(state.missing, seq, reach);
        if (state.waiting.length >= HELD_CAP) {
          this.release(state);
          return [];
        }
        const out = this.drain(state);
        this.arm(state);
        return out;
      }
      case 'seat_frames': {
        const slot = message.slot as SessionSlot | undefined;
        if (slot === undefined) return [message];
        const key = subjectKey({ session: slot });
        const state = this.streams.get(key);
        if (state === undefined || !state.anchored || this.settled(state)) return [message];
        state.origin = { slot, epoch: message.epoch ?? null };
        for (const frame of message.frames) {
          const carried = frame as { seq: number; span?: unknown };
          const span = typeof carried.span === 'number' && carried.span > 0 ? carried.span : 1;
          filled(state.missing, frame.seq, frame.seq + span - 1);
          // A repair for numbers already delivered - a range answered twice,
          // which the gate and the store can both ask for - has nowhere to
          // land; keeping it would hand the readers the same frame again on
          // release.
          if (frame.seq + span - 1 < state.expecting) continue;
          state.repairs.push({ seq: frame.seq, span, update: frame.update });
        }
        const out = this.drain(state);
        this.arm(state);
        return out;
      }
      case 'error': {
        if (message.what === 'seat_range' && message.seat !== undefined) {
          const key = subjectKey({ session: message.seat });
          const state = this.streams.get(key);
          if (state !== undefined) {
            // The numbers the client held came from an epoch that has moved
            // on; the readers re-read the seat, which answers for the new one.
            this.clear(state);
            state.expecting = 1;
            state.seenThrough = 0;
          }
        }
        return [message];
      }
      default:
        return [message];
    }
  }

  /** Forget every seat: a closed connection's stream is over. */
  reset(): void {
    for (const state of this.streams.values()) this.clear(state);
    this.streams.clear();
  }

  /**
   * Forget a seat this connection has let go: its numbers stop counting, and
   * the fleet news the coarse watches still deliver for it passes through.
   *
   * **An anchor alone outlives the subscription, and that is the cost this
   * closes.** A seat a reader visited and then left keeps its anchor for the
   * life of the connection, so the next fleet-news frame for it - a turn
   * finishing in a project nobody is looking at - opens a hole and asks for
   * the whole missed stretch; the ring answers only what it still holds
   * (4,000 frames), and above that the transcript answers with a whole-file
   * read (found in review, 2026-10-15). Frames held at the moment of release
   * are handed over rather than dropped, and a later subscribe anchors the
   * seat again from its own snapshot.
   */
  releaseSeat(slot: SessionSlot): void {
    const key = subjectKey({ session: slot });
    const state = this.streams.get(key);
    if (state === undefined) return;
    this.release(state);
    this.clear(state);
    state.anchored = false;
    state.epoch = null;
    state.expecting = 1;
    state.seenThrough = 0;
  }

  /** Whether nothing is in flight for this seat. */
  private settled(state: StreamState): boolean {
    return (
      state.missing.length === 0 &&
      state.waiting.length === 0 &&
      state.repairs.length === 0 &&
      state.deadline === null
    );
  }

  /** Record a hole and ask for it, once per range. */
  private open(state: StreamState, range: Range, slot: SessionSlot): void {
    cover(state.missing, range);
    if (covers(state.asked, range)) return;
    cover(state.asked, range);
    if (!this.deps.ask(slot, state.epoch, range.from, range.to)) {
      // No socket is open, so no answer is coming: nothing waits for one.
      this.release(state);
    }
  }

  /**
   * Emit, in strict sequence, everything the run can now deliver.
   *
   * Repaired frames go back inside `seat_frames` messages (maximal runs of
   * consecutive repaired numbers, cut by whatever frame comes next), because
   * that message is the call path that strips a repeat and keeps a repair
   * from arming the follow; waiting frames go out as the updates they were.
   */
  private drain(state: StreamState): ServerMessage[] {
    const out: ServerMessage[] = [];
    let batch: { seq: number; update: SessionUpdate }[] = [];
    const flushBatch = (): void => {
      if (batch.length === 0 || state.origin === null) return;
      out.push({
        kind: 'seat_frames',
        slot: state.origin.slot,
        epoch: state.origin.epoch,
        frames: batch,
      } as unknown as ServerMessage);
      batch = [];
    };

    for (;;) {
      const at = state.expecting;
      const repair = state.repairs.find((frame) => frame.seq <= at && at < frame.seq + frame.span);
      if (repair !== undefined) {
        state.repairs = state.repairs.filter((frame) => frame !== repair);
        batch.push({ seq: repair.seq, update: repair.update });
        state.expecting = repair.seq + repair.span;
        continue;
      }
      const waiting = state.waiting.find((frame) => frame.seq <= at && at < frame.seq + frame.span);
      if (waiting !== undefined) {
        flushBatch();
        state.waiting = state.waiting.filter((frame) => frame !== waiting);
        out.push({
          kind: 'update',
          seq: waiting.seq,
          span: waiting.span,
          update: waiting.update,
        });
        state.expecting = Math.max(state.expecting, waiting.seq + waiting.span);
        continue;
      }
      break;
    }
    flushBatch();
    return out;
  }

  /** Renew the hold's deadline, or settle it when nothing is pending. */
  private arm(state: StreamState): void {
    if (state.deadline !== null) clearTimeout(state.deadline);
    state.deadline = null;
    if (this.settled(state)) {
      state.asked.length = 0;
      return;
    }
    // Every missing range has already been asked for (`missing` only grows
    // through `open`, which asks in the same breath), so the wait below is
    // for answers in flight rather than a pending ask.
    state.deadline = setTimeout(() => this.release(state), HELD_DEADLINE_MS);
  }

  /** Release a hold as it stands, in sequence: an answer never came. */
  private release(state: StreamState): void {
    if (state.deadline !== null) clearTimeout(state.deadline);
    state.deadline = null;
    const frames = [
      ...state.repairs.map((frame) => ({ frame, repaired: true })),
      ...state.waiting.map((frame) => ({ frame, repaired: false })),
    ].sort((a, b) => a.frame.seq - b.frame.seq);
    state.repairs.length = 0;
    state.waiting.length = 0;
    state.missing.length = 0;
    state.asked.length = 0;
    if (frames.length === 0) return;
    const out: ServerMessage[] = [];
    let batch: { seq: number; update: SessionUpdate }[] = [];
    const flushBatch = (): void => {
      if (batch.length === 0 || state.origin === null) return;
      out.push({
        kind: 'seat_frames',
        slot: state.origin.slot,
        epoch: state.origin.epoch,
        frames: batch,
      } as unknown as ServerMessage);
      batch = [];
    };
    for (const { frame, repaired } of frames) {
      if (repaired) {
        batch.push({ seq: frame.seq, update: frame.update });
      } else {
        flushBatch();
        out.push({
          kind: 'update',
          seq: frame.seq,
          span: frame.span,
          update: frame.update,
        });
      }
      state.expecting = Math.max(state.expecting, frame.seq + frame.span);
    }
    flushBatch();
    this.deps.emit(out);
  }

  private clear(state: StreamState): void {
    if (state.deadline !== null) clearTimeout(state.deadline);
    state.deadline = null;
    state.missing.length = 0;
    state.asked.length = 0;
    state.waiting.length = 0;
    state.repairs.length = 0;
    state.origin = null;
  }
}
