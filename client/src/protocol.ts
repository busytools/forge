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
 * The protocol this client speaks.
 *
 * Fixed rather than negotiated: the server changes far more slowly than a
 * client's visuals do, and a difference is a fact the surface draws - the
 * rail footer carries both halves' build and socket numbers, and a mismatch
 * raises the notice beside it. Nothing refuses a connection over it.
 */
export const PROTOCOL_VERSION = 8;

/**
 * The release this client was built from, baked in by the build.
 *
 * `client/src-tauri/Cargo.toml` is where `just release` sets it, and this
 * is the only way the built app can know it: a bundle carries no manifest to
 * read.
 */
export const CLIENT_VERSION = __FORGE_CLIENT_VERSION__;

/** A greeting whose protocol differs from this client's own. */
export interface Skew {
  /** The protocol the greeting declared. */
  serverProtocol: number;
  /**
   * The build the greeting named, or `null` when it named none.
   *
   * `null` is the case for a server that predates the greeting's own release
   * fields; a server ahead of this client carries them.
   */
  serverVersion: string | null;
}

/**
 * The release part of a build stamp, without the sha the build adds.
 *
 * Exported because the tests word the same sentences from it: a message
 * naming a release to install names the bare number, never the stamp.
 */
export function releaseOf(version: string): string {
  const [release] = version.split(/[+ ]/);
  return release ?? version;
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
 * Both notice sites read from here, so the command and the halves that CAN be
 * named cannot be named at one site and forgotten at another.
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
    const release = releaseOf(CLIENT_VERSION);
    // "Reinstall both" is two commands, and this is the one branch where the
    // release to hand the client's installer is known: both halves are it.
    return (
      `this forge server and this client are both v${release}, but they speak protocols ` +
      `${skew.serverProtocol} and ${PROTOCOL_VERSION}: a mixed install of one build. Reinstall ` +
      `both: in the forge checkout run \`just install\` and \`just client-release ${release}\`, ` +
      'then restart.'
    );
  }
  return `this forge server is v${skew.serverVersion} (protocol ${skew.serverProtocol}); ${mine}. ${command}`;
}

/**
 * What a client can watch, and the address a subscription is held under.
 *
 * `dictate_models` and `extensions` are their pages' own reads: the pins
 * and catalogue only the models page draws, and the plugin inventory only
 * the extensions page draws.
 */
export type Subject = 'home' | { session: SessionSlot } | 'usage' | 'dictate_models' | 'extensions';

/**
 * A subscription's address as one string, which is what a store is keyed by.
 *
 * A subject is an object for a seat and a string for the rest, so a map keyed
 * on the subject itself would never match two equal slots. The string subjects
 * ARE their own key, which is the name the server serialised.
 *
 * The triple is joined with a NUL rather than a slash, which an org, project
 * or label may contain: two seats whose parts were shaped so the joined
 * strings matched would share one store, and each would draw the other's
 * snapshot.
 */
export function subjectKey(subject: Subject): string {
  if (typeof subject === 'string') return subject;
  const { org, project, label } = subject.session;
  return `session:${org}\u0000${project}\u0000${label}`;
}

/**
 * A core command: the variant's name around its own fields.
 *
 * A struct variant crosses as its name around a field bag - `{cancel:
 * {key}}` - and the core's one unit variant a page sends,
 * `DictateCatalogueCheck`, as its name alone, which is the shape serde writes
 * for a unit variant.
 *
 * Left unenumerated rather than written out here: every surface dispatches a
 * handful of the variants, and a union naming them would be a second copy of
 * an enum the server generates from. A caller builds the shape it means, and
 * the page that owns it is where that shape is stated.
 */
export type Command = string | Record<string, Record<string, unknown>>;

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

/** One frame of a repaired stretch: the number the seat minted for it, and the update itself. */
export interface SeatFrame {
  seq: number;
  update: SessionUpdate;
}

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

/**
 * What a document is, as the read detected it.
 *
 * `other` is kept as itself rather than bucketed - the lowercased bare
 * extension, or empty when the name carries none - so a viewer draws the
 * kind it was handed instead of one label for everything unknown.
 */
export type DocumentKind = 'markdown' | 'pdf' | 'text' | { other: string };

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
      /**
       * Whether this client can host the browser.
       *
       * Off unless the client says otherwise, and the declaration is a claim
       * about what this client can DO: one connection holds the role, every
       * browser ask routes to it, and a client that declared the capability
       * it does not have is sent asks it will answer with a failure - which
       * shows up as a session's tool call failing, not as the client's.
       */
      browser: boolean;
      /**
       * Whether this frame is the role DECLARE: the re-statement of the
       * browser capability a client makes as it claims the role.
       *
       * The server answers it with the subject's snapshot like any
       * subscribe, but holds NOTHING for it - no watch entry, no
       * attachment, no seat hold - because the client never counts it and
       * so would never give one back.
       */
      declare: boolean;
    }
  | { kind: 'unsubscribe'; what: Subject }
  | { kind: 'command'; command: Command; reply_to: number | null }
  | { kind: 'more'; conversation: SessionSlot; before: string | null; turns: number }
  /**
   * Ask for the frames a seat's stream emitted between two numbers.
   *
   * **Every frame the workspace emits for a seat is numbered on that seat's
   * own counter**, so a client that reads 5 and then 7 knows exactly one frame
   * went missing. `epoch` is the occupant those numbers belong to: a seat
   * whose occupant has changed has started counting again at 1, and a repair
   * across the two is refused rather than answered - so an `error` naming
   * `seat_range` means read the seat again.
   */
  | { kind: 'seat_range'; slot: SessionSlot; epoch: string | null; from: number; to: number }
  /**
   * One commit's own diff, for a review page's commit scopes.
   *
   * On demand rather than carried on the record: a branch can hold dozens of
   * commits, and every reader of a record would pay their content on every
   * read for a scope only one page draws. The answer is a `commit_diff`
   * message or an `error` naming it.
   */
  | { kind: 'commit_diff'; session: SessionSlot; sha: string }
  /**
   * Ask for one file's whole content at a revision, for the review page's
   * between-hunk expansion.
   *
   * The record's diff carries bounded hunks, so the lines between them are
   * exactly what it does not carry. `sha` names the scope's own revision
   * and `null` is the seat's branch tip, the new side of the all-changes
   * diff. The answer is a `file_content` message; a read that could not run
   * answers as itself rather than as an error.
   */
  | { kind: 'file_content'; session: SessionSlot; path: string; sha: string | null }
  /**
   * Ask for one document out of a seat's own tree, for a viewer's route.
   *
   * `path` resolves against the seat's working directory, and `reviewable`
   * is what a hand-off flagged: it rides the read back so the route knows
   * whether to offer comments. The answer is a `document_read` message or
   * an `error` naming it, and those are its only two.
   */
  | { kind: 'document_read'; session: SessionSlot; path: string; reviewable: boolean }
  /**
   * The host's answer to one `browser_ask`.
   *
   * `parts` are what the tool returned, in order; an image part's BYTES do
   * not cross here - they ride a binary frame under this same id, one frame
   * per image part in the order the parts are listed. `error` is the call's
   * failure, and a failed answer carries no parts.
   */
  | { kind: 'browser_answer'; id: number; parts: BrowserPart[]; error: string | null }
  /**
   * Take the browser role from whoever holds it, which is the force override
   * a second capable client offers. The server answers with a `browser_role`
   * frame either way, so a refused take is visible rather than silent.
   */
  | { kind: 'browser_take_role' };

/** What the server sends. */
export type ServerMessage =
  | {
      kind: 'greeting';
      version: number;
      /**
       * The build the server is, in the greeting because that is the only
       * channel both halves have before anything else crosses. Absent from
       * a server that predates the fields.
       */
      forge_version?: string;
      forge_version_short?: string;
      settings: ClientSettings;
    }
  | { kind: 'snapshot'; subject: Subject; data: unknown }
  /**
   * One frame of a seat's stream.
   *
   * `seq` is the number its OLDEST update was minted under, and `span` how
   * many of the seat's numbers the frame stands for: a run of token appends
   * crosses as the single frame they draw as, and counting only its oldest
   * would leave the client asking for numbers it had already drawn. Both are
   * absent from a frame belonging to no seat, and from a server older than the
   * fields.
   */
  | { kind: 'update'; update: SessionUpdate; seq?: number | null; span?: number }
  /**
   * A repaired stretch, in answer to `seat_range`: the frames numbered in
   * `from..=to`, oldest first, each carrying the number it was minted under
   * and the update in the same shape a live `update` carries.
   */
  | { kind: 'seat_frames'; slot: SessionSlot; epoch: string | null; frames: SeatFrame[] }
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
  /**
   * A page of turns.
   *
   * `reconciled` is the desktop's own mark: the Rust half sends the
   * newest window after a parked spell, and the mark is what tells it from
   * an answer to an ask (a park can kill the ask while its "in flight"
   * bookkeeping survives, and an unmarked page would then be spliced as an
   * older one). The web build never sees it.
   */
  | {
      kind: 'page';
      conversation: SessionSlot;
      turns: unknown[];
      cursor: string | null;
      reconciled?: boolean;
    }
  /**
   * The answer to `commit_diff`: one commit's own bounded diff, in the same
   * layer shape the record's content read carries, so both scopes draw
   * through one parse.
   */
  | { kind: 'commit_diff'; sha: string; files: unknown; truncated: boolean }
  /**
   * The answer to `file_content`: the file's lines at the revision asked
   * for, or why the read could not run.
   *
   * `session`, `path` and `sha` are named back so a page holding two
   * seats, two scopes or several files routes each answer to its own ask;
   * the content itself is narrowed where it enters, in `session/wire.ts`.
   */
  | {
      kind: 'file_content';
      session: SessionSlot;
      path: string;
      sha: string | null;
      content: unknown;
    }
  /**
   * One document, as the read answered: the seat it was read for, the
   * resolved absolute path, the detected kind, the bytes base64, the flag
   * the hand-off passed, the document's own hash, and the review it already
   * carries.
   *
   * `session` is named back so a page holding two seats routes each answer
   * to its own viewer - the same reason a refusal names its seat. `bytes`
   * is base64 because the wire is JSON, and `document_kind` because the
   * envelope is already tagged on `kind`. `content_hash` crosses as
   * decimal digits - JS numbers are exact only to 2^53 - and `threads` /
   * `reviews` are the document scope's own store read.
   */
  | {
      kind: 'document_read';
      session: SessionSlot;
      path: string;
      document_kind: DocumentKind;
      bytes: string;
      reviewable: boolean;
      content_hash: string;
      threads: unknown[];
      reviews: unknown[];
    }
  /** The answer to a command that asked for one, a refusal included. */
  | { kind: 'reply'; reply_to: number; body: unknown }
  /**
   * `seat` names the conversation a refusal belongs to where it belongs to
   * one: a `more` the server could not answer names its seat, so a client
   * holding several seats' asks drains only its own. Absent from a server that
   * predates the field, and from refusals that are about nothing seat-shaped.
   *
   * `dispatch_id` is the dispatch a refused command answered by - a press
   * that named one hears its refusal by name. `null` when the refused
   * command named none, and absent from a server that predates the field.
   */
  | {
      kind: 'error';
      what: string;
      why: string;
      /**
       * The conversation the refusal belongs to, or `null` where it belongs
       * to none. **The wire sends `null`, not an absent field**, so a reader
       * tests it as a value rather than for presence (#2420).
       */
      seat?: SessionSlot | null;
      dispatch_id?: number | null;
    }
  /**
   * One browser tool call, sent to the client that holds the browser role.
   *
   * The socket's first request in this direction. It is answered with a
   * `browser_answer` under this same `id`; nothing else pairs the two, so an
   * ask left unanswered is a session's tool call waiting.
   */
  | { kind: 'browser_ask'; id: number; seat: SessionSlot; tool: string; args: unknown }
  /**
   * Whether THIS connection holds the browser role: sent when a capable
   * declare is granted, and when a force-take takes the role away. Before
   * this frame a client knew only that it COULD host, and a strip that went
   * on saying "you" after losing the role would be a lie its reader acts on.
   */
  | { kind: 'browser_role'; hosting: boolean };

/**
 * One part of a browser tool's answer, as it crosses the socket.
 *
 * An image names its mime type here and its bytes travel in their own binary
 * frame: base64 inside this JSON would pay a third again for a screenshot.
 */
export type BrowserPart = { type: 'text'; text: string } | { type: 'image'; mime_type: string };

/**
 * One part as a host holds it: the same shape, with an image's bytes in hand.
 *
 * The socket strips the bytes into their frame, so what a handler returns is
 * this and what crosses is {@link BrowserPart}.
 */
export type BrowserAnswerPart =
  { type: 'text'; text: string } | { type: 'image'; mime_type: string; bytes: Uint8Array };

/** One browser ask, as the handler that answers it reads it. */
export interface BrowserAsk {
  /** The id the answer must come back under. */
  id: number;
  /** The seat whose session asked. */
  seat: SessionSlot;
  /** Upstream's own tool name, unprefixed. */
  tool: string;
  /** The arguments the CLI sent, exactly as it sent them. */
  args: unknown;
}

/**
 * What a host answers an ask with: the tool's parts, or the reason it failed.
 *
 * A failure is an ANSWER, not a silence - the session's tool call returns it
 * as the reason the call did not happen, which is what a person reads.
 */
export type BrowserAnswer = { parts: BrowserAnswerPart[] } | { error: string };

/**
 * The largest page one `more` asks for, and the page the history walks use.
 *
 * A page carries whole turns, so the ask is a count rather than a position:
 * the boundary can never land inside a turn, and the server errs toward
 * repeating a row rather than toward a gap. The column sizes the reader's
 * own pull from what it has measured; this is that size's ceiling, and the
 * count the dispatch and compaction walks keep asking for.
 */
export const MORE_TURNS = 20;
