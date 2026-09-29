/**
 * What a client and the server exchange, mirroring
 * `crates/forge-server/src/transport/envelope.rs`.
 *
 * **The envelope and the payload are tagged differently, and that is the
 * trap.** `ClientMessage` and `ServerMessage` are tagged on `kind`, which
 * sits beside a message's own fields. The `Command` and `SessionUpdate`
 * they carry are not: the core's own enums are externally tagged, so a
 * variant is its name around its own value rather than a `kind` field
 * among them. Writing a payload the envelope's way parses nowhere, and the
 * server refuses it as a message it does not know.
 *
 * `docs/book/src/socket.md` is the prose for this, and it is the page a
 * client author reads first.
 */

import type { ClientSettings, SessionSlot } from './wire/types';

/**
 * The protocol this client speaks, which the greeting must agree with.
 *
 * Fixed rather than negotiated, because the server changes far more slowly
 * than a client's visuals do: either a client speaks this version or it does
 * not, and a mismatch fails plainly instead of silently.
 */
export const PROTOCOL_VERSION = 1;

/** What a client can watch, and the address a subscription is held under. */
export type Subject = 'home' | { session: SessionSlot } | 'usage';

/**
 * A subscription's address as one string, which is what a store is keyed by.
 *
 * A subject is an object for a seat and a string for the other two, so a map
 * keyed on the subject itself would never match two equal slots.
 *
 * The triple is joined with a NUL rather than a slash, which an org, project
 * or label may contain: two seats whose parts were shaped so the joined
 * strings matched would share one store, and each would draw the other's
 * snapshot.
 */
export function subjectKey(subject: Subject): string {
  if (subject === 'home') return 'home';
  if (subject === 'usage') return 'usage';
  const { org, project, label } = subject.session;
  return `session:${org}\u0000${project}\u0000${label}`;
}

/**
 * A core command: the variant's name around its own fields.
 *
 * `Command` has 33 variants and every one of them is a struct variant, so
 * the inner value is always a field bag - `{cancel: {key}}`, never a bare
 * `"cancel"`.
 *
 * Left unenumerated rather than written out here: every surface dispatches a
 * handful of the 33, and a union naming them would be a second copy of an
 * enum the server generates from. A caller builds the shape it means, and
 * the page that owns it is where that shape is stated.
 */
export type Command = Record<string, Record<string, unknown>>;

/**
 * One `SessionUpdate`, for a subscription that covers it.
 *
 * Three shapes, because the enum has three kinds of variant and 56 variants
 * in all: a unit variant is its name alone, a struct variant is its name
 * around a field bag, and the one newtype variant is its name around the
 * value inside it.
 *
 * Open for the same reason as `Command`, and one more: 56 variants cross
 * here, and the page that draws a subject is the only place that knows which
 * of them it acts on.
 */
export type SessionUpdate = string | { [variant: string]: unknown };

/**
 * The seat an update is addressed to, or `null` for one addressed to no seat.
 *
 * This is the core's own `SessionUpdate::slot`, which is `Some(key)` for the
 * 41 variants that carry a `key: SessionSlot` and `None` for everything else -
 * so reading `key` IS reading that method, and the field is never anything but
 * a slot.
 *
 * It is what a client routes by: a subscription is a seat, and an update's
 * seat is the seat it belongs to.
 */
export function slotOf(update: SessionUpdate): SessionSlot | null {
  if (typeof update === 'string') return null;
  const payload: unknown = Object.values(update)[0];
  // A variant's fields arrive as JSON, and this is the one place that reaches
  // into them.
  const key: unknown = (payload as { key?: unknown } | null)?.key;
  return isSlot(key) ? key : null;
}

function isSlot(value: unknown): value is SessionSlot {
  if (value === null || typeof value !== 'object') return false;
  const fields = value as Record<string, unknown>;
  return (
    typeof fields['org'] === 'string' &&
    typeof fields['project'] === 'string' &&
    typeof fields['label'] === 'string'
  );
}

/** What a client sends. */
export type ClientMessage =
  | {
      kind: 'subscribe';
      what: Subject;
      /**
       * Whether this client can answer the prompts it is shown.
       *
       * Off unless the client says otherwise, because the core parks a turn
       * on the reply of whoever registered as answering, so a client counted
       * as able to answer a prompt it cannot display hangs the turn rather
       * than failing it. It is declared on the FIRST subscribe and stands
       * for the connection.
       */
      answering: boolean;
    }
  | { kind: 'unsubscribe'; what: Subject }
  | { kind: 'command'; command: Command; reply_to: number | null }
  | { kind: 'more'; conversation: SessionSlot; before: string | null; turns: number };

/** What the server sends. */
export type ServerMessage =
  | { kind: 'greeting'; version: number; settings: ClientSettings }
  | { kind: 'snapshot'; subject: Subject; data: unknown }
  | { kind: 'update'; update: SessionUpdate }
  /** A page of one conversation, in answer to `more`, with the cursor for the next. */
  | { kind: 'page'; conversation: SessionSlot; rows: unknown[]; cursor: string | null }
  /** The answer to a command that asked for one, a refusal included. */
  | { kind: 'reply'; reply_to: number; body: unknown }
  | { kind: 'error'; what: string; why: string };

/**
 * How many turns one `more` asks for.
 *
 * A page carries whole turns, so the ask is a count rather than a position:
 * the boundary can never land inside a turn, and the server errs toward
 * repeating a row rather than toward a gap.
 */
export const MORE_TURNS = 20;
