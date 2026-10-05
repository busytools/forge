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
 * than a client's visuals do: either a client speaks a version or it does
 * not, and a mismatch fails plainly instead of silently. What is tolerated
 * below this one is `MIN_PROTOCOL`, and nothing above it is.
 */
export const PROTOCOL_VERSION = 5;

/**
 * The oldest protocol this client reads.
 *
 * A step back is tolerated because the read is proven rather than because a
 * skew is believed harmless: `wire/floor.test.ts` folds a committed
 * protocol-4 payload through this client's own read path, so the floor is
 * one where "it still reads" is checked rather than assumed. A bump keeps
 * this where it is until the new step is covered the same way, which is why
 * it is a literal rather than `PROTOCOL_VERSION - 1`.
 */
export const MIN_PROTOCOL = 4;

/**
 * The release this client was built from, baked in by the build.
 *
 * `client/src-tauri/Cargo.toml` is where `just release` sets it, and this
 * is the only way the built app can know it: a bundle carries no manifest to
 * read.
 */
export const CLIENT_VERSION = __FORGE_CLIENT_VERSION__;

/** A greeting below this client's own protocol, and within the floor. */
export interface Skew {
  /** The protocol the greeting declared. */
  serverProtocol: number;
  /**
   * The build the greeting named, or `null` when it named none.
   *
   * `null` is the case for a server one step back, which predates the
   * greeting's own release fields; a server ahead of this client carries
   * them.
   */
  serverVersion: string | null;
}

/** The release part of a build stamp, without the sha the build adds. */
function releaseOf(version: string): string {
  const [release] = version.split(/[+ ]/);
  return release ?? version;
}

/** Whether this client reads a server speaking `version`. */
export function readableProtocol(version: number): boolean {
  return version >= MIN_PROTOCOL && version <= PROTOCOL_VERSION;
}

/** The greeting's skew: the build it named, and the protocol it speaks. */
export function skewOf(greeting: Extract<ServerMessage, { kind: 'greeting' }>): Skew | null {
  if (greeting.version === PROTOCOL_VERSION) return null;
  return {
    serverProtocol: greeting.version,
    // Narrowed here rather than in the sentence, for the same reason
    // `settingsFrom` narrows beside it: the greeting crosses as blind JSON,
    // and a value that is not a string would reach the text as `v[object
    // Object]` at the one moment the text matters.
    serverVersion: releaseFrom(greeting.forge_version_short, greeting.forge_version),
  };
}

/** The first of the two stamps that is a release, or `null` when neither is. */
function releaseFrom(short: unknown, long: unknown): string | null {
  for (const stamp of [short, long]) {
    if (typeof stamp === 'string' && stamp !== '') return stamp;
  }
  return null;
}

/**
 * One sentence for a protocol skew: what the wire carries, and the way out.
 *
 * Both refusal sites and every notice read from here, so the command and the
 * halves that CAN be named cannot be named at one site and forgotten at
 * another.
 */
export function skewMessage(skew: Skew): string {
  const command = 'In the forge checkout run `just install` and restart forge.';
  const mine = `this client is v${CLIENT_VERSION} (protocol ${PROTOCOL_VERSION})`;
  // A server ahead of this client is the half this client cannot fix by
  // rebuilding the server: the half to update is this one, and the command
  // is the one that installs a client.
  if (skew.serverProtocol > PROTOCOL_VERSION) {
    const named =
      skew.serverVersion === null || skew.serverVersion === '' ? null : skew.serverVersion;
    const theirs =
      named === null
        ? `this forge server speaks protocol ${skew.serverProtocol}`
        : `this forge server is v${named} (protocol ${skew.serverProtocol})`;
    const command =
      named === null
        ? 'in the forge checkout run `just client-release <version>` with the release the server ' +
          'reports, and restart the app.'
        : `in the forge checkout run \`just client-release ${releaseOf(named)}\` and restart the app.`;
    return `${theirs}; ${mine}. This client is the half that is behind: ${command}`;
  }
  if (skew.serverVersion === null || skew.serverVersion === '') {
    return `this forge server speaks protocol ${skew.serverProtocol}; ${mine}. ${command}`;
  }
  if (releaseOf(skew.serverVersion) === releaseOf(CLIENT_VERSION)) {
    return (
      `this forge server and this client are both v${releaseOf(CLIENT_VERSION)}, but they speak ` +
      `protocols ${skew.serverProtocol} and ${PROTOCOL_VERSION}: a mixed install of one build. ` +
      `Reinstall both. ${command}`
    );
  }
  return `this forge server is v${skew.serverVersion} (protocol ${skew.serverProtocol}); ${mine}. ${command}`;
}

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
 * `Command` has 37 variants and every one of them is a struct variant, so
 * the inner value is always a field bag - `{cancel: {key}}`, never a bare
 * `"cancel"`.
 *
 * Left unenumerated rather than written out here: every surface dispatches a
 * handful of the 37, and a union naming them would be a second copy of an
 * enum the server generates from. A caller builds the shape it means, and
 * the page that owns it is where that shape is stated.
 */
export type Command = Record<string, Record<string, unknown>>;

/**
 * One `SessionUpdate`, for a subscription that covers it.
 *
 * Three shapes, one per kind of variant the enum carries: a unit variant is
 * its name alone, a struct variant is its name around a field bag, and the one
 * newtype variant is its name around the value inside it.
 *
 * Open for the same reason as `Command`, and one more: every variant crosses
 * here, and the page that draws a subject is the only place that knows which
 * of them it acts on.
 */
export type SessionUpdate = string | { [variant: string]: unknown };

/**
 * The seat an update is addressed to, or `null` for one addressed to no seat.
 *
 * This is the core's own `SessionUpdate::slot`, which is `Some(key)` for the
 * variants that carry a `key: SessionSlot` and `None` for everything else - so
 * reading `key` IS reading that method, and the field is never anything but a
 * slot.
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

/** One input forge can record from, as the socket names it. */
export interface DictateDevice {
  /**
   * The stable identity, which is what a pick sends back: names collide between
   * two identical interfaces and change when a user renames one.
   */
  id: string;
  /** The human label a picker draws. Not an identity. */
  name: string;
  /** Whether the system would pick this one when asked for no particular device. */
  is_default: boolean;
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
  | { kind: 'more'; conversation: SessionSlot; before: string | null; turns: number }
  /**
   * Ask for the inputs forge can record from.
   *
   * On demand rather than carried: the walk opens the microphone stack, and a
   * record is encoded per request and re-sent on every reconnect, so a field
   * would be a permission check per frame and per connection. The answer is a
   * `devices` message or an `error` naming it, and those are its only two.
   */
  | { kind: 'devices' };

/** What the server sends. */
export type ServerMessage =
  | {
      kind: 'greeting';
      version: number;
      /**
       * The build the server is, in the greeting because that is the only
       * channel both halves have before a client refuses anything. Absent
       * from a server that predates the fields, which is every server a
       * skew is against today.
       */
      forge_version?: string;
      forge_version_short?: string;
      settings: ClientSettings;
    }
  | { kind: 'snapshot'; subject: Subject; data: unknown }
  | { kind: 'update'; update: SessionUpdate }
  /**
   * A page of one conversation, in answer to `more`, with the cursor for the
   * next.
   *
   * Each turn carries its own key and its own MESSAGES - the CLI's frames, the
   * same shape the session snapshot's `conversation` carries. How a run of
   * tool calls groups inside a turn is a drawing decision, so it is the
   * client's; what the server keeps is the boundary between turns, because
   * the paging contract is built on it and a page that split one would leave
   * a client stitching half a turn to the other half.
   */
  | { kind: 'page'; conversation: SessionSlot; turns: unknown[]; cursor: string | null }
  /**
   * The inputs forge can record from, in answer to `devices`.
   *
   * `configured` is the `forge.toml` pin and NOT what is in force: a pick moves
   * the process's input for the rest of the run, and what it moved to rides the
   * home snapshot's `dictate.device`. A picker drawing `configured` as the
   * current input is wrong from its first pick onward.
   */
  | { kind: 'devices'; devices: DictateDevice[]; configured: string | null }
  /** The answer to a command that asked for one, a refusal included. */
  | { kind: 'reply'; reply_to: number; body: unknown }
  /**
   * `seat` names the conversation a refusal belongs to where it belongs to
   * one: a `more` the server could not answer names its seat, so a client
   * holding several seats' asks drains only its own. Absent from a server that
   * predates the field, and from refusals that are about nothing seat-shaped.
   */
  | { kind: 'error'; what: string; why: string; seat?: SessionSlot };

/**
 * How many turns one `more` asks for.
 *
 * A page carries whole turns, so the ask is a count rather than a position:
 * the boundary can never land inside a turn, and the server errs toward
 * repeating a row rather than toward a gap.
 */
export const MORE_TURNS = 20;
