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
 * numbers `span` it covers. A gap wider than one number is a hole; the frames
 * above it are held, a `seat_range` ask goes out for the gap, and the answer
 * releases the hold in sequence - the repaired frames first (they arrive
 * inside the `seat_frames` message), then the held ones, oldest first.
 *
 * What this layer deliberately does NOT do: drop anything. A frame behind the
 * newest number still reaches the readers (the store skips it as drawn; the
 * chat strips a repeat by row identity), and a frame carrying no number - a
 * delivery the server forged, or anything from a server older than the field -
 * passes through untouched.
 */

import { slotOf, subjectKey, type ServerMessage, type SessionUpdate } from '../protocol';
import type { SessionSlot } from '../wire/types';

/** How many frames one seat holds above a hole before it releases them. */
const HELD_CAP = 256;

/**
 * How long a hold waits for its answer before releasing the frames.
 *
 * **A hold with no answer coming must not freeze the seat.** A socket that
 * dropped mid-ask, or a range no window could fill, would otherwise hold
 * forever; the release is the fallback the readers' own rules (the store's
 * second-layer hold, the chat's identity) already know how to take.
 */
const HELD_DEADLINE_MS = 5000;

/** One frame held above a hole, with the numbers it stands for. */
interface Held {
  seq: number;
  span: number;
  message: ServerMessage;
}

interface StreamState {
  /** The occupant the numbers belong to, off the seal or the answer. */
  epoch: string | null;
  /** The next number this stream expects to deliver. */
  expecting: number;
  /** A range ask is in flight for this seat. */
  asking: boolean;
  held: Held[];
  deadline: ReturnType<typeof setTimeout> | null;
}

/** What a gate needs from the connection it orders. */
export interface GateDeps {
  /** Send a range ask; `false` when the socket is not open. */
  ask: (slot: SessionSlot, epoch: string | null, from: number, to: number) => boolean;
  /** Hand messages to every consumer, in order. */
  emit: (messages: ServerMessage[]) => void;
}

function stateOf(streams: Map<string, StreamState>, key: string): StreamState {
  let state = streams.get(key);
  if (state === undefined) {
    state = { epoch: null, expecting: 1, asking: false, held: [], deadline: null };
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

export class StreamGate {
  private readonly streams = new Map<string, StreamState>();

  constructor(private readonly deps: GateDeps) {}

  /**
   * One inbound message; whatever should reach the consumers now, in order.
   *
   * A message that arms a hold returns `[]` and reaches the consumers later,
   * through `emit`, when its repair lands (or the deadline fires).
   */
  inbound(message: ServerMessage): ServerMessage[] {
    switch (message.kind) {
      case 'snapshot': {
        const subject = message.subject as { session?: SessionSlot };
        if (subject?.session === undefined) return [message];
        const key = subjectKey({ session: subject.session });
        const state = stateOf(this.streams, key);
        const header = (message.data as { header?: { session_id?: unknown; seq?: unknown } } | null)
          ?.header;
        const occupant = typeof header?.session_id === 'string' ? header.session_id : null;
        if (occupant !== null && occupant !== state.epoch) {
          // A new occupant counts from 1 again, hold and all.
          this.clear(state);
          state.epoch = occupant;
          state.expecting = 1;
        }
        if (typeof header?.seq === 'number') {
          // **The answer is the authority on where the stream stands**, the
          // same rule the store reads it by: a restart under the same
          // occupant counts from 1 again, and an answer that says so must
          // lower what this expects - a monotonic maximum would leave the
          // restarted run's frames reading as already drawn.
          state.expecting = header.seq;
        }
        return [message];
      }
      case 'update': {
        const slot = slotOf(message.update);
        if (slot === null) return [message];
        const key = subjectKey({ session: slot });
        const state = stateOf(this.streams, key);

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
          if (typeof occupant === 'string') state.epoch = occupant;
          // The seal is the wipe signal itself: it goes out at once.
          return [message];
        }

        const seq = typeof message.seq === 'number' ? message.seq : null;
        if (seq === null) return [message];
        const span = typeof message.span === 'number' && message.span > 0 ? message.span : 1;
        const reach = seq + span - 1;

        if (state.asking) {
          state.held.push({ seq, span, message });
          if (state.held.length >= HELD_CAP) this.release(state);
          return [];
        }
        if (seq > state.expecting) {
          // The hole: hold this frame and everything after, ask for the gap.
          state.asking = true;
          state.held.push({ seq, span, message });
          state.deadline = setTimeout(() => this.release(state), HELD_DEADLINE_MS);
          if (!this.deps.ask(slot, state.epoch, state.expecting, seq - 1)) {
            this.release(state);
          }
          return [];
        }
        // A successor, or a frame already drawn (the readers decide which).
        state.expecting = Math.max(state.expecting, reach + 1);
        return [message];
      }
      case 'seat_frames': {
        const slot = message.slot as SessionSlot | undefined;
        if (slot === undefined) return [message];
        const key = subjectKey({ session: slot });
        const state = this.streams.get(key);
        if (state === undefined || (!state.asking && state.held.length === 0)) {
          return [message];
        }
        // The repaired frames arrive inside the message, so the readers apply
        // them first; the held frames follow, oldest first. A repaired frame
        // stands for its own span when the server carries one (a folded run
        // repaired from the seat's window), one number otherwise.
        for (const frame of message.frames) {
          const carried = frame as { seq: number; span?: unknown };
          const span = typeof carried.span === 'number' && carried.span > 0 ? carried.span : 1;
          state.expecting = Math.max(state.expecting, frame.seq + span);
        }
        if (state.deadline !== null) clearTimeout(state.deadline);
        state.asking = false;
        state.deadline = null;
        const held = state.held.splice(0).sort((a, b) => a.seq - b.seq);
        for (const frame of held) {
          state.expecting = Math.max(state.expecting, frame.seq + frame.span);
        }
        return [message, ...held.map((frame) => frame.message)];
      }
      case 'error': {
        if (message.what === 'seat_range' && message.seat !== undefined) {
          const key = subjectKey({ session: message.seat });
          const state = this.streams.get(key);
          if (state !== undefined) {
            this.clear(state);
            state.expecting = 1;
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

  /** Release a hold as it stands, in sequence: the answer never came. */
  private release(state: StreamState): void {
    if (state.deadline !== null) clearTimeout(state.deadline);
    const held = state.held.splice(0).sort((a, b) => a.seq - b.seq);
    state.asking = false;
    state.deadline = null;
    if (held.length === 0) return;
    for (const frame of held) {
      state.expecting = Math.max(state.expecting, frame.seq + frame.span);
    }
    this.deps.emit(held.map((frame) => frame.message));
  }

  private clear(state: StreamState): void {
    if (state.deadline !== null) clearTimeout(state.deadline);
    state.deadline = null;
    state.asking = false;
    state.held.length = 0;
  }
}
