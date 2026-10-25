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
export const PROTOCOL_VERSION = 12;

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
 * `board` is the board page's and the session rail's read: every project's
 * rows, schedules and connector sets (#2517). `dictate_models` and
 * `extensions` are their pages' own reads: the pins and catalogue only the
 * models page draws, and the plugin inventory only the extensions page draws.
 */
export type Subject =
  'home' | 'board' | { session: SessionSlot } | 'usage' | 'dictate_models' | 'extensions';

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

/** The subjects whose key is the subject itself, which is what the wire sends. */
const NAMED_SUBJECTS: readonly string[] = [
  'home',
  'board',
  'usage',
  'dictate_models',
  'extensions',
];

/**
 * The subject a store key names, or `null` for a key this client does not
 * know. The inverse of [`subjectKey`], used where the wire names no subject
 * and the side that attributed a refusal has to say which one it was.
 */
export function subjectFromKey(key: string): Subject | null {
  if (NAMED_SUBJECTS.includes(key)) return key as Subject;
  const seat = key.startsWith('session:') ? key.slice('session:'.length) : null;
  if (seat === null) return null;
  const [org, project, label] = seat.split('\u0000');
  if (org === undefined || project === undefined || label === undefined) return null;
  return { session: { org, project, label } };
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

/** What one tab is doing, as the strip's mark says it. */
export type BrowserTabState = 'in_use' | 'idle' | 'held';

/** One tab as a client's browser strip draws it. */
export interface BrowserTabRow {
  /** The host's tab number, which frames and input speak. */
  number: number;
  title: string;
  url: string;
  /** The session whose calls drive it. */
  owner: SessionSlot;
  /** The server's own word, kept whole: a state this page has not seen is
   *  drawn as itself rather than as a neighbour it does not mean. */
  state: string;
}

/** One profile's tabs and how the profile itself is doing. */
export interface BrowserTabList {
  profile: string;
  /** The id the binary frames carry for the profile. */
  id: number;
  /** Whether its browser host is up. */
  up: boolean;
  /** Whether a driver is attached and running. */
  connected: boolean;
  /** When the idle clock closes it, as Unix epoch milliseconds. */
  closing_at_ms: number | null;
  rows: BrowserTabRow[];
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
   * Attach this connection to one of a seat's browser tabs, as a viewer.
   *
   * `tab` is the tab to drive; `null` is the asking seat's current tab, else
   * its profile's first. Attaching again replaces what this connection was
   * viewing. The answer is a `browser_view` naming the profile's wire id and
   * the tab, then a `browser_nav_state`, then frames.
   */
  | {
      kind: 'browser_view';
      seat: SessionSlot;
      tab: number | null;
      /** Answers the attach when the sender waits; the shell's does. */
      reply_to?: number | null;
    }
  /**
   * The tabs of this seat's profile, for a client's strip.
   *
   * Answered through `reply_to` with `{ok: true, tabs}`, where `tabs` is a
   * [`BrowserTabList`]; the connection then follows every change as a pushed
   * `browser_tabs`.
   */
  | { kind: 'browser_tabs'; seat: SessionSlot; reply_to?: number | null }
  /** Stop viewing the attached profile: the takeover lowered. */
  | { kind: 'browser_detach' }
  /**
   * One input event from the controlling viewer. **The wire carries the
   * gesture, not CDP**: the server maps it to the engine's own input.
   */
  | { kind: 'browser_input'; seq: number; event: BrowserInputEvent }
  /** Take the controller role for the viewed surface, or give it back. */
  | { kind: 'browser_control'; take: boolean }
  /** Move the viewed page through its own history: the bar's arrows. */
  | { kind: 'browser_nav'; back: boolean };

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
       *
       * The conversation fold is the one reader that ACTS on null: a
       * seatless `subscribe` refusal may belong to any seat. The other three
       * - `session/stream`, `session/live` and `doc/live` - read the field
       * only on `seat_range` and `document_read` refusals, which the server
       * always names by seat, so their null test is type-driven rather than a
       * case the wire produces.
       */
      seat?: SessionSlot | null;
      /**
       * The subscription a refusal belongs to, when THIS side synthesized the
       * frame.
       *
       * **The wire's own errors name no subject** - an `error` carries `what`
       * and `why`, and a subscribe refusal is attributed by the side that
       * remembers what it asked for. That side says which subject it was, so
       * a page does not act on another subject's refusal; absent means the
       * frame came off the wire and the reader falls back to its own store.
       */
      subject?: Subject;
      dispatch_id?: number | null;
    }
  /**
   * A profile's viewer state: which viewer controls it, and how many are
   * attached. `id` is the profile's wire id, the same one the binary frames
   * carry; `viewer` is this connection's own, which is what `controller`
   * names when this surface holds the role.
   */
  | {
      kind: 'browser_view';
      id: number;
      profile: string;
      /** The tab this state is for; a frame for another tab is not this one's. */
      tab: number;
      controller: number | null;
      viewers: number;
      viewer: number;
    }
  /** Where the viewed tab's history sits: what the bar's arrows may do. */
  | {
      kind: 'browser_nav_state';
      id: number;
      tab: number;
      can_back: boolean;
      can_forward: boolean;
    }
  /** The viewed tab's browser stopped, and why. */
  | { kind: 'browser_surface_failed'; id: number; tab: number; reason: string }
  /**
   * The viewed profile's tabs, pushed once the connection asked with
   * `browser_tabs` and then once per change.
   */
  | ({ kind: 'browser_tabs' } & BrowserTabList);

/** The modifier keys a person held for one input gesture. */
export interface InputModifiers {
  alt: boolean;
  control: boolean;
  meta: boolean;
  shift: boolean;
}

/**
 * One input event a controlling viewer sends, in CSS pixels for the page's
 * viewport.
 *
 * **A gesture, not CDP**: the server turns a press into the engine's own
 * input, and the method names stay there.
 */
export type BrowserInputEvent =
  | {
      kind: 'mouse';
      action: 'move' | 'press' | 'release';
      x: number;
      y: number;
      /** 0 left, 1 right, 2 middle. */
      button: number;
      clicks: number;
      modifiers: InputModifiers;
    }
  | {
      kind: 'wheel';
      x: number;
      y: number;
      delta_x: number;
      delta_y: number;
      modifiers: InputModifiers;
    }
  | {
      kind: 'key';
      down: boolean;
      key: string;
      code: string;
      windows_vk: number;
      modifiers: InputModifiers;
    }
  | { kind: 'text'; text: string };

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
