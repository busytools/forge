/**
 * The connection to a running forge: one socket, the stores it answers into,
 * and the reconnect that puts it back.
 *
 * **The socket carries no history.** A reconnect is a new connection - the
 * server's registry of who is watching what lived on the old one - so
 * everything held here is asked for again, the same number of times it was
 * asked for before, and the stores keep what they had until the fresh answer
 * arrives. Blanking them first would show the reader a page emptying and
 * refilling on every blip.
 */

import { READ_BOUND_MS, ReadBounds } from './read-bound';
import { refused } from './refusals';
import { StreamGate } from './session/stream';
import {
  MORE_TURNS,
  skewOf,
  slotOf,
  subjectFromKey,
  subjectKey,
  type BrowserInputEvent,
  type ClientMessage,
  type Command,
  type ServerMessage,
  type SessionUpdate,
  type Skew,
  type Subject,
} from './protocol';
import { noteSnapshot, noteSubscribe, type PerfRecord } from './perf';
import { Stores, type Store } from './stores';
import { coversHome } from './wire/fleet';
import { settingsFrom } from './wire/types';
import type { ClientSettings, SessionSlot } from './wire/types';

/** Where a connection is in its life. */
export type ConnectionStatus = 'connecting' | 'open' | 'closed';

export interface Connection {
  /**
   * Watch a subject, and get the store it is answered into.
   *
   * `answering` says whether this client has the surface to answer prompts,
   * and it is off unless said otherwise: the core parks a turn on the reply of
   * whoever registered as answering, so a client counted as able to answer a
   * prompt it has no surface for hangs the turn rather than failing it. **The
   * role belongs to the CONNECTION, not to the subject**, and a client that
   * can answer states it once, usually on the first subscribe it makes.
   *
   * **It is a safeguard rather than the thing that keeps an ask alive.** The
   * core cancels an ask at birth only when no subscriber anywhere can answer
   * it, so a client that says nothing does not itself keep an ask alive. The
   * one install this covers is a serve with no answerer attached. A page that
   * cannot prompt - a probe, a headless observer - must not declare the role:
   * the core parks on whoever did, and nothing there would ever answer.
   *
   * Counted rather than idempotent, because the server counts it too: two
   * subscribes to one subject are two subscriptions, and one unsubscribe must
   * not take the seat out of the set the other is still watching.
   */
  subscribe(what: Subject, options?: { answering?: boolean; review?: boolean }): Store;
  unsubscribe(what: Subject): void;
  /**
   * Ask again for a subject this connection already holds.
   *
   * A pair rather than a bare `subscribe`: the server counts a subscription
   * per subscribe and removes one per unsubscribe, and that count is what it
   * filters every update against, so a second subscribe with nothing given
   * back would grow the list for the life of the connection.
   *
   * **Answers whether the ask really went**, which a page's own refresh has
   * no use for and the read bound has: a connection that is down or closed
   * has nothing to ask on, and a bound must not re-arm for a read it did not
   * ask.
   */
  /**
   * Ask for a subject again. `review` is the review page's own read: the
   * server refreshes the branch diff's base before it encodes the record,
   * which every other read does not wait on.
   */
  refresh(what: Subject, options?: { review?: boolean }): boolean;
  /**
   * Tell the server which seat is on screen.
   *
   * **Remembered and re-sent on every reconnect**: the server's showing is
   * per connection, and a drop ends it. `null` is "nothing of mine is on
   * screen", not "I am not attached".
   */
  showing(seat: SessionSlot | null): void;
  /**
   * Ask again for a subject whose store this client still holds.
   *
   * **A subscription's two halves die differently.** A socket drop takes the
   * server's half and leaves the store here open; asking again through
   * `subscribe` would open a second store, and its count would drift away
   * from the server's. This re-asks with the role the subject already
   * declared, and puts it back in the list a reconnect re-asks.
   */
  relist(what: Subject, options?: { answering?: boolean; review?: boolean }): void;
  /**
   * Change a subject's held declaration **without asking the server for
   * anything at all**.
   *
   * The scope belongs to the page holding the seat, and a page that lets its
   * review read go has to say so: a plain re-ask would put a declaration on
   * the wire, and left unsaid the transport would keep re-asking as the
   * review read long after the page went - one base fetch and pinned scan per
   * reviewed seat, on every reconnect nobody asked for.
   */
  declare(what: Subject, options: { review: boolean }): void;
  /**
   * Give a subject back entirely: every store reader this side holds for it,
   * and every subscription the server holds for it.
   *
   * **The count is the SERVER's, not the page's.** A resume asks once for
   * the seat it re-attaches, so this side may hold two readers where the
   * wire holds one; walking the readers would send an unsubscribe the server
   * does not hold and leave one it does.
   */
  release(what: Subject): void;
  /**
   * Send a core command.
   *
   * Answers with a promise for the five commands whose outcome rides a reply
   * because no update carries it, and `null` for every other command, whose
   * outcome arrives through the subscription.
   *
   * **The choice is not the caller's.** The server refuses a `reply_to` on a
   * command that does not answer through one, and refuses its absence on a
   * command that does, dispatching in neither case - so which of the two this
   * is gets decided here, from the variant's name, rather than left for every
   * page to remember.
   *
   * **A call while the socket is closed THROWS**, synchronously, before
   * anything is registered. The command is dropped, no answer is coming to
   * carry that news, and the throw is the only channel left. It throws
   * rather than rejecting because a promise is then never created for a
   * command that did not go, and a rejection nobody awaits is an error
   * nobody sees.
   *
   * **So the promise and the throw are two different cases, not one.** A
   * command that went answers with a promise; a command that did not go
   * throws instead. Read `Promise<unknown> | null` as "went, answering
   * through a reply or through the subscription" - never as "may reject".
   * The rejection is the other case: a command already sent, whose
   * connection then closes or whose reply the server never sends - an older
   * server answers a command it does not know with an `error` frame naming
   * no reply - has the promise it returned rejected, because a caller
   * waiting on a channel that stays empty is worse off than one told.
   *
   * A caller that dispatches one of the five owes the returned promise an
   * answer: nothing else carries the outcome.
   */
  dispatch(command: Command, at?: SessionSlot): Promise<unknown> | null;
  /**
   * Ask for older turns of one conversation, answering whether the ask went.
   *
   * `false` means the socket was not open and no page is coming. A page that
   * set a loading flag on the strength of this call would otherwise clear it
   * on a reply that never arrives.
   *
   * A page reaches `onMessage` rather than a promise: `Page` carries no id to
   * pair it with its ask, so two asks for one conversation could not be told
   * apart, and asking twice at once is ordinary rather than a mistake.
   */
  more(conversation: SessionSlot, before?: string | null, turns?: number): boolean;
  /**
   * Ask for one commit's own diff, for a review page's commit scopes.
   *
   * Answered by a `commit_diff` message on `onMessage` or an `error` naming
   * it; `false` means the socket was not open and no answer is coming.
   */
  commitDiff(session: SessionSlot, sha: string): boolean;
  /**
   * Ask for one file's whole content at a revision, for the review page's
   * between-hunk expansion.
   *
   * `sha` is the scope's own revision and `null` the seat's branch tip. The
   * answer is a `file_content` message, read off `onMessage`; `false` means
   * the socket was not open and no answer is coming.
   */
  fileContent(session: SessionSlot, path: string, sha: string | null): boolean;
  /**
   * Ask for one document, by the seat whose tree its path resolves against.
   *
   * One ask for one read, answered by a `document_read` message carrying
   * the seat back - so a page holding two seats routes each answer to its
   * own viewer - or an `error` naming it. `false` means the socket was not
   * open and no answer is coming.
   */
  readDocument(session: SessionSlot, path: string, reviewable: boolean): boolean;
  /**
   * Ask a seat for the frames its stream emitted between two numbers.
   *
   * The repair a client runs on a gap it read in the sequence, and silent in
   * both directions: nothing on a page announces it, and the answer's frames
   * fold through the same path a live frame does. `false` means the socket was
   * not open and no answer is coming - which costs nothing, because the frames
   * a reconnect's snapshot carries are the whole record and say where the
   * stream stands.
   */
  seatRange(slot: SessionSlot, epoch: string | null, from: number, to: number): boolean;
  /**
   * Send one binary frame: dictation audio, and nothing else the socket
   * carries.
   *
   * Answers whether it went. `false` means the socket is not open, and the
   * take holding that frame keeps it rather than losing it - which is what
   * the ring is for.
   */
  frame(bytes: Uint8Array): boolean;
  /**
   * One input event from the controlling viewer.
   *
   * **The wire carries the gesture, not CDP**: a press, a wheel, a key or
   * inserted text, with the modifier keys held. The server turns it into the
   * engine's own input.
   */
  browserInput(event: BrowserInputEvent): boolean;
  /**
   * The page's own perf records, carried over the socket for a client with no
   * shell to write them.
   *
   * Answers whether the batch went: `false` means the socket is down, and the
   * caller drops it rather than queueing an unbounded backlog. **Absent on a
   * connection with no socket of its own** - the desktop bridge, whose page
   * writes through the shell's invoke instead.
   */
  clientPerf?(records: PerfRecord[]): boolean;
  /**
   * Hear each browser frame the server relays, as its host's own wire: the
   * profile id, the sequence, the view size, the key flag and the encoded
   * pixels. The phone draws them on a canvas; the desktop hands them to its
   * shell, which has no page listener.
   */
  onBrowserFrame(fn: (frame: BrowserFrame) => void): () => void;
  /** Every message the server sent, unparsed by anything here. Answers a function that stops listening. */
  onMessage(fn: (message: ServerMessage) => void): () => void;
  /**
   * Watch where the connection is in its life, which a page draws from: a
   * page that keeps its contents across a drop still has to say it is
   * reconnecting, or it draws pre-drop data as though it were live.
   */
  onStatus(fn: (status: ConnectionStatus) => void): () => void;
  store(what: Subject): Store | undefined;
  /** The client's mark, theme and font, from the greeting - the only place a client gets them. */
  settings(): ClientSettings | null;
  /**
   * What the last greeting said that this client's own protocol does not
   * agree with, or `null` when they agree.
   *
   * Set for any server whose protocol differs from this client's own: the
   * surfaces draw the notice and name the two builds from here rather than
   * from a number they have no build for.
   */
  skew(): Skew | null;
  /**
   * The protocol the last greeting carried, or `null` before one lands.
   *
   * The number behind `skew()`, kept whether or not there is a skew: the
   * footer draws both sides of the pair, so a match is a fact to show and
   * not the absence of a notice.
   */
  serverProtocol(): number | null;
  status(): ConnectionStatus;
  close(): void;
}

/**
 * The commands a PAGE dispatches that answer through a reply.
 *
 * The server's own set is `answers_through_a_reply`, which also carries
 * commands the server dispatches itself, like a hand-off's answer. These are
 * the page commands with nothing behind them on the wire: a client that omits
 * `reply_to` for one of them has no second way to learn what happened.
 */
const ANSWERS_THROUGH_A_REPLY = new Set([
  'spawn_worker',
  'despawn_worker',
  'upsert_review_thread',
  'delete_review_thread',
  'submit_review',
]);

/**
 * The bound one reply-carrying dispatch is given. The server answers in
 * milliseconds; a reply that outlives this is the version-skew case an
 * `error` frame answers without a reply id, or a wedge - and a caller parked
 * on it would never return. **A despawn is exempt**: its server path removes a
 * worktree and reaps a branch first, which can run for minutes (#1922, #1113),
 * so no finite bound is honest.
 */
const REPLY_BOUND_MS = 10_000;

/**
 * The bound on a browser attach, which is longer than a command's.
 *
 * **A cold profile starts a host and a driver before it can answer**, and a
 * measured cold start is about 14 s - so the command bound would report every
 * first attach as failed and then log the answer nobody waited for.
 */
const ATTACH_BOUND_MS = 30_000;

/** The bound this command gets, or `null` for a despawn. */
function replyBoundFor(variant: string, bound: number): number | null {
  return variant === 'despawn_worker' ? null : bound;
}

/** One command's caller, waiting for the reply that answers it. */
type PendingReply = {
  resolve: (body: unknown) => void;
  reject: (why: Error) => void;
  /** Absent for a command with no bound (a despawn). */
  timer?: ReturnType<typeof setTimeout>;
};

/**
 * How long to wait before the first re-ask, and the ceiling it doubles up to.
 *
 * Exported because `socket.test.ts` derives its absence windows from it: a
 * re-ask after a drop arrives on this beat, so raising the beat must widen
 * those windows rather than silently outrun them.
 */
export const RETRY_MS = 50;
/**
 * Exported because `session/live.test.ts` derives its wait deadline from it:
 * raising the ceiling must not silently shorten every wait in that file.
 */
export const MAX_RETRY_MS = 2000;

/**
 * A command's variant name.
 *
 * Externally tagged commands have exactly one key, so anything else is a
 * caller that spread two of them together - and the server would refuse the
 * result as a message it does not know, which reads at the far end as a
 * command that silently never ran. Thrown here, where the caller can see it.
 *
 * A unit variant IS its name, and it has no keys to count.
 */
function variantOf(command: Command): string {
  if (typeof command === 'string') return command;
  const names = Object.keys(command);
  const [only] = names;
  if (names.length !== 1 || only === undefined) {
    throw new Error(`a command is one variant, and this one has ${names.length}`);
  }
  return only;
}

/** The kind tag a browser frame carries, which is the wire's own. */
const BROWSER_FRAME_TAG = 1;

/**
 * One relayed browser frame, as the host encoded it.
 *
 * The server re-encodes nothing: the payload is the host's own WebP and the
 * header states the size it was painted at, so a viewer decodes rather than
 * stretches.
 */
export interface BrowserFrame {
  /** The profile's wire id, from the `browser_view` that named it. */
  profile: number;
  /** The profile's tab this frame paints: the other half of the filter. */
  tab: number;
  /** Monotonic per tab; a newer frame supersedes an older one. */
  seq: number;
  width: number;
  height: number;
  /** A full-quality frame: the first of a surface, a resize, an attach. */
  key: boolean;
  /** The page's own viewport, the space a gesture's point is sent in. **Not
   *  the frame's pixels**: the encode's raster sits between them. */
  page_width: number;
  page_height: number;
  /** The encoded pixels, untouched by the server. */
  bytes: Uint8Array;
}

/**
 * The browser's own client messages, as a page that draws the server's browser
 * sends them.
 *
 * **These are not core commands.** `browser_view` and `browser_tabs` answer
 * through replies of their own, and the rest are fire-and-forget assertions
 * about what this viewer is doing. A connection without them is the desktop
 * bridge, whose shell drives the browser through its native overlay instead.
 */
export interface BrowserWire {
  /**
   * Attach this connection to one of a seat's browser tabs, as a viewer.
   *
   * Answers the profile and tab the server actually attached - a hand-off
   * that named no tab resolves to the seat's current one - or rejects with
   * the server's own sentence.
   */
  browserView(seat: SessionSlot, tab: number | null): Promise<BrowserAttach>;
  /**
   * Read a seat's profile tabs.
   *
   * **The ask is the subscription**: the answer is the rows as they stand,
   * and every change after it is pushed as a `browser_tabs`.
   */
  browserTabs(seat: SessionSlot): Promise<unknown>;
  /** Give the attached profile up: the takeover lowered. */
  browserDetach(): boolean;
  /** Take the controller role for the viewed surface, or give it back. */
  browserControl(take: boolean): boolean;
  /** Move the viewed page through its history. */
  browserNav(back: boolean): boolean;
  /** Lay the page out to this viewer's surface, in device pixels. */
  browserResize(width: number, height: number, scale: number): boolean;
}

/** The attach a `browser_view` answered with. */
export interface BrowserAttach {
  profile: string;
  tab: number;
}

/**
 * One attach's reply, narrowed once: the resolved pair, or the server's own
 * sentence for why it could not attach.
 */
function attachOf(body: unknown): BrowserAttach {
  if (typeof body !== 'object' || body === null) {
    throw new Error('the attach answered nothing this page can read');
  }
  const held = body as Record<string, unknown>;
  if (held['ok'] !== true) {
    throw new Error(
      typeof held['why'] === 'string' && held['why'] !== ''
        ? held['why']
        : 'the browser could not be opened',
    );
  }
  if (typeof held['profile'] !== 'string' || held['profile'] === '') {
    throw new Error('the attach named no profile');
  }
  if (typeof held['tab'] !== 'number' || !Number.isInteger(held['tab'])) {
    throw new Error('the attach named no tab');
  }
  return { profile: held['profile'], tab: held['tab'] };
}

/** One binary message, decoded as a browser frame, when it is one. */
function browserFrame(bytes: Uint8Array): BrowserFrame | null {
  // The kind tag, then the host's own 18-byte header, then the pixels.
  if (bytes.length < 19 || bytes[0] !== BROWSER_FRAME_TAG) return null;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  return {
    profile: view.getUint16(1, true),
    tab: view.getUint16(3, true),
    seq: view.getUint32(5, true),
    width: view.getUint16(9, true),
    height: view.getUint16(11, true),
    key: bytes[14] === 1,
    page_width: view.getUint16(15, true),
    page_height: view.getUint16(17, true),
    bytes: bytes.subarray(19),
  };
}

/**
 * One line about something the client could not do.
 *
 * Deliberately not a logging framework: this layer's failures are otherwise
 * silent - a refused subscribe, a socket that dropped, a reply nobody asked
 * for, a store that will not keep an address - and each is a fact a reader of
 * the console can act on.
 */
export function report(what: string, why: unknown): void {
  console.warn(`forge client: ${what}`, why);
}

export function connect(
  url: string,
  boundMs: number = READ_BOUND_MS,
  replyBoundMs: number = REPLY_BOUND_MS,
): Connection & BrowserWire {
  const stores = new Stores();
  /**
   * The bound on a subscription's first read: a subject still loading when
   * it runs out is asked again, so a snapshot lost on a live socket is not
   * a wait with nothing behind it.
   */
  const bounds = new ReadBounds(stores, (what) => refresh(what), boundMs);
  /**
   * What to ask for again on a reconnect, in the order it was first asked.
   *
   * `review` rides here for the same reason the role does: the read a page
   * wanted must survive the drop, and a review open that reconnects as an
   * ordinary record would label its base unverified for a check it never
   * asked for.
   */
  const held = new Map<string, { subject: Subject; answering: boolean; review: boolean }>();
  const listeners = new Set<(message: ServerMessage) => void>();
  const statuses = new Set<(status: ConnectionStatus) => void>();
  /** The page's viewers of the relayed frames: the phone's canvas. */
  const frameListeners = new Set<(frame: BrowserFrame) => void>();
  const pending = new Map<number, PendingReply>();
  /**
   * The subscribes that have gone and not been answered, oldest first, which
   * is the order the server answers them in.
   *
   * A refusal carries `what` and `why` and no subject, so this is what
   * attributes one. It is a queue of asks rather than a scan for stores that
   * have no snapshot, because a `refresh` over a store that is already ready
   * is refused with nothing in that state to find - and the store then keeps
   * drawing its pre-drop data with nothing saying so, which is the state the
   * third state exists to prevent.
   */
  const awaiting: string[] = [];

  let socket: WebSocket | null = null;
  /**
   * The ordering gate of the connection that is open, or the last one that
   * was.
   *
   * Built with each socket rather than once, because its state is a stream's
   * and a reconnect's stream starts over; this reference is what lets an
   * unsubscribe outside the socket's own handler tell the current gate that
   * a seat has been let go.
   */
  let gate: StreamGate | null = null;
  let status: ConnectionStatus = 'connecting';
  let settings: ClientSettings | null = null;
  let protocolSkew: Skew | null = null;
  /** The greeting's own number, kept whether or not it skews. */
  let greetingProtocol: number | null = null;
  /**
   * The seat this client has on screen, re-sent on every reconnect.
   *
   * The server's showing is per connection, and a drop ends it; `null` is
   * "nothing of mine is on screen" rather than "not attached".
   */
  let shown: SessionSlot | null = null;
  let nextReplyId = 1;
  /** The wire sequence the viewer's gestures carry, minted per connection. */
  let inputSeq = 0;
  let retry: ReturnType<typeof setTimeout> | null = null;
  let retryDelay = RETRY_MS;

  /** The one place openness is decided, so no two callers can disagree. */
  function isOpen(): boolean {
    return status === 'open' && socket !== null && socket.readyState === WebSocket.OPEN;
  }

  function move(next: ConnectionStatus): void {
    if (status === next) return;
    status = next;
    for (const fn of statuses) fn(next);
  }

  function sendNow(message: ClientMessage): void {
    if (!isOpen()) throw new Error('the socket is not open');
    socket?.send(JSON.stringify(message));
    // **The write's own mark (#2513).** Taken after the send, so the gap a
    // switch pays is measured to the frame leaving this side. A held
    // subscription is not the browser role's declare - this client has no
    // such role - so the flag is false.
    if (message.kind === 'subscribe') noteSubscribe(message.what, false);
  }

  function failPending(why: string): void {
    for (const { reject, timer } of pending.values()) {
      if (timer !== undefined) clearTimeout(timer);
      reject(new Error(why));
    }
    pending.clear();
  }

  /**
   * One update into the stores it belongs to.
   *
   * A seat's update goes to that seat's store, and to the home store when the
   * home's covering rule (`wire/fleet.ts`) says the snapshot can carry its
   * change. That rule is narrower than the server's delivery: a keyless
   * update reaches every home subscriber, and only the ones with a home field
   * are worth a read.
   */
  function route(update: SessionUpdate): void {
    const slot = slotOf(update);
    if (slot !== null) stores.get({ session: slot })?.push(update);
    if (coversHome(update)) stores.get('home')?.push(update);
  }

  /**
   * A refusal, given to the store it belongs to.
   *
   * `Error` carries `what` and `why` and no subject, so the subject comes
   * from this side: a subscribe is the only message that is answered by
   * creating a store, and the server answers in the order it was asked, so
   * the oldest subscription still waiting on an answer is the refused one.
   */
  function refuse(why: string): Subject | null {
    const [key] = awaiting.splice(0, 1);
    if (key === undefined) {
      report('the server refused a subscribe this client cannot account for', why);
      return null;
    }
    const store = stores.byKey(key);
    if (store === undefined) {
      report(`the server refused ${key}, which this client no longer holds`, why);
      return null;
    }
    // A refusal is an answer: nothing is owed on that subject any more.
    bounds.clearKey(key);
    // **A refusal the store already holds is not news.** A page that asks a
    // refused subject again on a beat meets the same words every time, and
    // reporting them again would warn once per retry for no new information.
    const held = store.state();
    if (held.kind !== 'refused' || held.why !== why) {
      store.refuse(why);
      report(`the subscription to ${key} was refused`, why);
    }
    return subjectFromKey(key);
  }

  /**
   * How many subscriptions the server holds for each subject on this
   * connection.
   *
   * **It is the wire's count, not the stores'**: a reconnect asks each
   * subscription exactly once, and two things ask on an open - the
   * connection's loop for the subjects it holds, and the page's `relist` for
   * the seat it has on screen, which puts that seat back in the held list as
   * it asks. This is how the loop knows what has already asked, and how
   * `release` knows what is left to give back.
   */
  const subscribed = new Map<string, number>();

  /** One subscribe on the wire, remembered as outstanding until it is answered. */
  function askFor(what: Subject, answering: boolean, review = false): void {
    sendNow({ kind: 'subscribe', what, answering, review });
    const key = subjectKey(what);
    awaiting.push(key);
    subscribed.set(key, (subscribed.get(key) ?? 0) + 1);
    // The read this ask owes is bounded from here: nothing else would ask
    // again for a snapshot that simply never came. A review read is armed with
    // no bound at all, which also clears any timer an ordinary read of the
    // same subject left standing: the server waits for its base fetch with no
    // cap, so a timer would ask a second whole record under one still
    // legitimately running.
    bounds.arm(what, review ? null : undefined);
  }

  /** A subject the server has answered, which is no longer outstanding. */
  function answered(what: Subject): void {
    const at = awaiting.indexOf(subjectKey(what));
    if (at >= 0) awaiting.splice(at, 1);
    bounds.clear(what);
  }

  /**
   * Every outstanding ask for one subject, dropped.
   *
   * A refresh re-asks the SAME subscription - the server counts the pair
   * evenly - so the bookkeeping has to be net-zero too. Left alone, an ask
   * whose answer was lost keeps its entry, the re-ask adds another, and a
   * later refusal (which carries no subject and takes the oldest key) would
   * flip the store of the ask that is already spent rather than the one the
   * server refused.
   *
   * **The Rust half's purge is narrower than this rule**: its `unhold` drops
   * the key only when the last unsubscribe takes a subject's hold count to
   * zero, so a subject two callers hold keeps the surplus there - a gap of
   * its own, filed separately, where this covers the web transport.
   */
  function forget(what: Subject): void {
    const key = subjectKey(what);
    for (let at = awaiting.length - 1; at >= 0; at -= 1) {
      if (awaiting[at] === key) awaiting.splice(at, 1);
    }
  }

  function handle(message: ServerMessage): void {
    switch (message.kind) {
      case 'greeting': {
        settings = settingsFrom(message.settings);
        // Recorded on every greeting rather than only the first: a page left
        // open across a forge upgrade reconnects with a different pair, and
        // the surfaces draw the notice from here. Nothing refuses.
        protocolSkew = skewOf(message);
        greetingProtocol = typeof message.version === 'number' ? message.version : null;
        return;
      }
      case 'snapshot':
        answered(message.subject);
        stores.get(message.subject)?.set(message.data);
        return;
      case 'update':
        route(message.update);
        return;
      case 'reply': {
        const waiting = pending.get(message.reply_to);
        if (waiting === undefined) {
          // The two sides disagree about the reply space: nothing this
          // client asked for is waiting on that id.
          report(
            `a reply arrived for id ${message.reply_to}, which nothing is waiting on`,
            message,
          );
          return;
        }
        pending.delete(message.reply_to);
        if (waiting.timer !== undefined) clearTimeout(waiting.timer);
        waiting.resolve(message.body);
        return;
      }
      case 'error':
        if (message.what === 'subscribe' || message.what === 'client_message') {
          // **An unknown subject comes back as `client_message`**, because the
          // server could not read the frame that named it; the refused
          // subscription is the one this side is still waiting on.
          const refused = refuse(message.why);
          if (refused !== null) message.subject = refused;
        } else {
          report(`the server refused a ${message.what}`, { why: message.why, seat: message.seat });
        }
        return;
      // `page` is the conversation's and `commit_diff` the review canvas': a
      // surface that draws one reads it from `onMessage`.
      case 'page':
      case 'commit_diff':
      case 'file_content':
      case 'document_read':
        return;
      // The browser's own frames are the shell's or a phone canvas': this
      // connection knows them and draws none of them itself.
      case 'browser_view':
      case 'browser_nav_state':
      case 'browser_surface_failed':
      case 'browser_tabs':
        return;
      // `seat_frames` is the conversation's too, and the same way: a repaired
      // stretch folds through the chat's own listener, so this switch knows
      // the frame and draws nothing of it. Reported as unknown it would read
      // as a frame the client could not draw - which is exactly what the
      // report below is for.
      case 'seat_frames':
        return;
      // A frame whose kind this client does not know is REPORTED rather than
      // dropped in silence: a message that arrived and drew nothing is
      // indistinguishable from one that never arrived.
      default:
        report('the server sent a frame this client does not know', message);
        return;
    }
  }

  function open(): void {
    retry = null;
    if (status === 'closed') return;

    let next: WebSocket;
    try {
      next = new WebSocket(url);
    } catch (error) {
      // An address the constructor will not take is not one a retry will
      // take either, but a retry is what keeps the connection trying when
      // the failure was transient - and without this the timer's throw would
      // leave the status saying `connecting` with nothing in flight.
      report(`could not open ${url}`, error);
      move('connecting');
      scheduleRetry();
      return;
    }
    // Frames arrive as their bytes, not as a Blob: a canvas draws the bytes.
    next.binaryType = 'arraybuffer';
    socket = next;

    next.onopen = () => {
      if (next !== socket) return;
      subscribed.clear();
      move('open');
      retryDelay = RETRY_MS;
      for (const { subject, answering, review } of [...held.values()]) {
        // **A seat here was opened while the socket was DOWN.** `onclose`
        // takes the seats the old connection carried out of this list, so a
        // visited seat is never re-asked by a resume - `session/live.ts`
        // re-attaches the shown one on the status change, and the rest
        // re-attach on their next click. What is left is a seat the reader
        // opened during the drop, whose first answer is still owed.
        //
        // Once per subscription, which is how many times the server was asked
        // before the drop: one unsubscribe drops one of its entries, so
        // re-asking once for a subject held twice would leave its count lower
        // than this side's, and a later unsubscribe would take the subject
        // away from a caller still drawing it. What this connection has
        // already asked - the shown seat, re-attached above - is not asked
        // again.
        const already = subscribed.get(subjectKey(subject)) ?? 0;
        for (let remaining = stores.count(subject) - already; remaining > 0; remaining -= 1) {
          askFor(subject, answering, review);
        }
      }
      if (shown !== null) sendNow({ kind: 'showing', seat: shown });
    };

    // **The ordering gate, and what it feeds.** Every consumer of this
    // connection reads through `deliver`, so a frame held above a hole is
    // held for all of them at once - the connection's own handling and the
    // pages' listeners alike (`session/stream`).
    const deliver = (message: ServerMessage): void => {
      try {
        handle(message);
      } catch (why) {
        // A frame that parses but is not the shape this client expects lands
        // here rather than out of the handler, where it would take the frames
        // after it and the listeners after it.
        report('a frame this client could not handle', why);
      }
      for (const fn of listeners) {
        // One page's listener throwing must not silence the pages after it.
        try {
          fn(message);
        } catch (why) {
          report('a message listener threw', why);
        }
      }
    };
    const opened = new StreamGate({
      ask: (slot, epoch, from, to) => {
        if (!isOpen()) return false;
        sendNow({ kind: 'seat_range', slot, epoch, from, to });
        return true;
      },
      emit: (messages) => {
        for (const message of messages) deliver(message);
      },
      follows: (slot) => held.has(subjectKey({ session: slot })),
    });
    gate = opened;

    next.onmessage = (event) => {
      if (next !== socket) return;
      // **A binary message is a relayed browser frame.** The phone's canvas
      // draws it; a page with no viewer ignores it, which is the desktop,
      // where the shell took the same bytes off its own socket.
      if (event.data instanceof ArrayBuffer) {
        const frame = browserFrame(new Uint8Array(event.data));
        if (frame === null) {
          // **A binary frame this page cannot read is said**, not dropped:
          // an unknown kind or a short header would otherwise vanish with no
          // trace at all.
          report('a binary frame was not a browser frame', event.data.byteLength);
          return;
        }
        for (const fn of frameListeners) {
          try {
            fn(frame);
          } catch (why) {
            report('a browser frame listener threw', why);
          }
        }
        return;
      }
      const raw = String(event.data);
      let message: ServerMessage;
      try {
        // The boundary: a frame is narrowed once, here, rather than by each
        // reader. A frame this client cannot read is reported rather than
        // thrown, because a throw out of this handler would take every frame
        // after it as well.
        const at = performance.now();
        message = JSON.parse(raw) as ServerMessage;
        // **The parse's own mark (#2513).** A large snapshot's parse is a
        // main-thread cost between a click and its subscribe's answer, so the
        // record carries the frame's length and the time the parse took.
        if (message.kind === 'snapshot') {
          noteSnapshot(message.subject, raw.length, performance.now() - at);
        }
      } catch (why) {
        report('the server sent a frame this client could not read', why);
        return;
      }
      // **One ordered stream, for every reader.** The seat store, the chat
      // fold and this connection's own bookkeeping all consume the frames,
      // so the ordering lives here rather than in any one of them: a hole
      // holds the frames above it until its repair lands, and every reader
      // sees the same sequence (`session/stream`).
      for (const ordered of opened.inbound(message)) {
        deliver(ordered);
      }
    };

    next.onclose = () => {
      if (next !== socket) return;
      socket = null;
      // A dropped socket's holds die with it: the reconnect's answer is the
      // whole record, and the numbers start wherever it says.
      opened.reset();
      if (status === 'closed') return;
      move('connecting');
      // A command that was in flight has no answer coming: the reply died
      // with the connection that would have carried it. So did every ask -
      // and `onopen` asks again, so leaving them queued would put stale keys
      // in front of the live ones and hand a later refusal to a store the
      // server had already answered.
      failPending('the socket dropped before answering');
      awaiting.length = 0;
      // **The drop took every server-side subscription with it.** The seat
      // subjects are the client's own to re-open - `session/live.ts`
      // re-attaches the shown one - so they leave this list here; the home
      // and board are re-asked by `onopen` like any other subject.
      for (const [key, entry] of [...held.entries()]) {
        if (typeof entry.subject !== 'string') held.delete(key);
      }
      scheduleRetry();
    };

    // An error is always followed by a close, which is where the retry is
    // scheduled - this reports it, because a socket that keeps failing is
    // otherwise indistinguishable from one that is merely slow.
    next.onerror = (event) => {
      if (next !== socket) return;
      report(`the socket to ${url} failed`, event);
    };
  }

  function scheduleRetry(): void {
    retry = setTimeout(open, retryDelay);
    retryDelay = Math.min(retryDelay * 2, MAX_RETRY_MS);
  }

  function subscribe(what: Subject, options?: { answering?: boolean; review?: boolean }): Store {
    if (status === 'closed') {
      // Nothing replays a subscribe made after the socket went, so a live
      // store here would promise a snapshot that is never coming.
      return stores.refused(what, 'this connection is closed');
    }

    const answering = options?.answering ?? false;
    const review = options?.review ?? false;
    const key = subjectKey(what);
    const store = stores.open(what);

    const existing = held.get(key);
    if (existing === undefined) {
      held.set(key, { subject: what, answering, review });
    } else {
      if (answering) {
        // The core's stream only ever escalates, so a later answering
        // subscribe raises the role the reconnect re-establishes.
        existing.answering = true;
      }
      // **The review read follows the page, so it is last-ask-wins.** A page
      // that let its review read go must be able to take the declaration
      // back; the role only rises, this one does not.
      existing.review = review;
    }

    if (isOpen()) {
      const declaration = held.get(key);
      askFor(what, declaration?.answering ?? answering, declaration?.review ?? review);
    }
    return store;
  }

  function unsubscribe(what: Subject): void {
    if (stores.get(what) === undefined) return;
    const last = stores.close(what);
    const key = subjectKey(what);
    if (last) held.delete(key);
    // **One entry per call, and only while this connection holds one.** A
    // dropped socket took the server's entries with it, so a seat released
    // after a resume - before its click re-attached it - has nothing to give
    // back; sending anyway would be silent on the wire and would leave this
    // side's count claiming a subscription the server never had, which the
    // next release and the next resume would both act on.
    const open = subscribed.get(key) ?? 0;
    if (open > 0) {
      if (open === 1) subscribed.delete(key);
      else subscribed.set(key, open - 1);
      if (isOpen()) sendNow({ kind: 'unsubscribe', what });
    }
    if (last) {
      // And the gate stops sequencing it: a left seat's news passes through
      // rather than opening a hole over a history nobody is reading.
      if (typeof what !== 'string') gate?.releaseSeat(what.session);
    }
  }

  /**
   * Give a subject back entirely: every store reader, and every server
   * subscription this connection holds for it.
   *
   * **The count is the wire's.** See [`Connection.release`].
   */
  function release(what: Subject): void {
    const key = subjectKey(what);
    // **The outstanding ask stays until its answer lands.** The server
    // answers in order, so this subject's unanswered subscribe is the head of
    // the queue; dropping it here would hand its refusal to the NEXT store.
    // A refusal that lands after the release finds no store and is dropped,
    // which is the right home for it.
    while (stores.get(what) !== undefined) stores.close(what);
    const open = subscribed.get(key) ?? 0;
    subscribed.delete(key);
    held.delete(key);
    if (isOpen()) {
      for (let left = open; left > 0; left -= 1) sendNow({ kind: 'unsubscribe', what });
    }
    // The gate stops sequencing it: a released seat's news passes through
    // rather than opening a hole over a history nobody is reading.
    if (typeof what !== 'string') gate?.releaseSeat(what.session);
  }

  /**
   * Open or close the seat on screen, as the route sees it.
   *
   * The server spends a seat's marks on this message and never on a
   * subscription, so it is the only thing that says the reader is looking.
   */
  function showing(seat: SessionSlot | null): void {
    shown = seat;
    if (isOpen()) sendNow({ kind: 'showing', seat });
  }

  /**
   * Ask again for a subject whose store this client still holds.
   *
   * **A subscription's two halves die differently.** A socket drop takes the
   * server's half and leaves the store here open; asking again through
   * `subscribe` would open a second store, and its count would drift away
   * from the server's. This re-asks with the role the seat already declared,
   * and puts the subject back in the list a reconnect re-asks.
   */
  function relist(what: Subject, options?: { answering?: boolean; review?: boolean }): void {
    const key = subjectKey(what);
    const existing = held.get(key);
    const answering = existing?.answering === true || options?.answering === true;
    const review = options?.review === true;
    held.set(key, { subject: what, answering, review });
    if (isOpen()) askFor(what, answering, review);
  }

  /** The held declaration only: nothing on the wire, no store, no reader. */
  function declare(what: Subject, options: { review: boolean }): void {
    const existing = held.get(subjectKey(what));
    if (existing !== undefined) existing.review = options.review;
  }

  /** `true` when the pair went, which is what a bound re-arms on. */
  function refresh(what: Subject, options?: { review?: boolean }): boolean {
    // Nothing to ask when the socket is down: the reconnect re-subscribes
    // everything held, and that answer is fresher than this ask would be.
    if (!isOpen()) return false;
    forget(what);
    sendNow({ kind: 'unsubscribe', what });
    // **Net zero on the wire's count**: the unsubscribe above gives one
    // entry back and the ask below takes one again, so a refresh must
    // decrement here or every refresh would leave this side counting one
    // more subscription than the server holds - and `release` would then
    // send frames for entries that do not exist.
    const key = subjectKey(what);
    const open = subscribed.get(key) ?? 0;
    if (open > 0) {
      if (open === 1) subscribed.delete(key);
      else subscribed.set(key, open - 1);
    }
    const declaration = held.get(key);
    // **A re-ask that states no scope keeps the held one.** The bound's
    // retry for a store still loading says nothing, and turning it into an
    // ordinary record would undo the review read it is retrying.
    const review = options?.review ?? declaration?.review ?? false;
    if (declaration !== undefined) declaration.review = review;
    askFor(what, declaration?.answering ?? false, review);
    return true;
  }

  /**
   * A reply channel for one message, with the bound its caller is given.
   *
   * Registered before the message goes, because a refusal can come back as
   * fast as the send returns. A reply that never comes has no frame to resolve
   * it - an older server answers an unknown message with an `error` naming no
   * reply id - so the caller's only other way out is the timer.
   */
  function asked(
    bound: number | null,
    what: string,
  ): { replyTo: number; answer: Promise<unknown> } {
    const replyTo = nextReplyId;
    nextReplyId += 1;
    const answer = new Promise<unknown>((resolve, reject) => {
      const entry: PendingReply = { resolve, reject };
      if (bound !== null) {
        entry.timer = setTimeout(() => {
          if (!pending.delete(replyTo)) return;
          reject(new Error(`the server did not answer the ${what}`));
        }, bound);
      }
      pending.set(replyTo, entry);
    });
    return { replyTo, answer };
  }

  /** One message on its way with no reply: `false` when nothing went. */
  function sendClient(message: ClientMessage): boolean {
    if (!isOpen()) return false;
    sendNow(message);
    return true;
  }

  function dispatch(command: Command, at?: SessionSlot): Promise<unknown> | null {
    // Before anything is registered, so a command that never went leaves no
    // promise behind for a later failure to reject.
    if (!isOpen()) {
      // The throw is what callers catch and report; this is what the reader
      // sees: the loss noted for the seat the click was made in (#1638). The
      // seat is the caller's to state - a command's own payload names its
      // TARGET, which is another seat for a rail's close - and a dispatch the
      // caller gives no seat for (a command no column sent) notes nothing.
      if (at !== undefined) refused(at);
      throw new Error('the socket is not open');
    }

    const variant = variantOf(command);
    if (!ANSWERS_THROUGH_A_REPLY.has(variant)) {
      sendNow({ kind: 'command', command, reply_to: null });
      return null;
    }

    const bound = replyBoundFor(variant, replyBoundMs);
    const { replyTo, answer } = asked(bound, 'command');
    try {
      sendNow({ kind: 'command', command, reply_to: replyTo });
    } catch (why) {
      // `JSON.stringify` is the one thing between the registration and the
      // send that can throw - a BigInt in a command - and an entry left
      // behind would be rejected later as though the command had gone.
      const registered = pending.get(replyTo);
      if (registered?.timer !== undefined) clearTimeout(registered.timer);
      pending.delete(replyTo);
      throw why;
    }
    return answer;
  }

  open();

  return {
    subscribe,
    unsubscribe,
    refresh,
    showing,
    relist,
    declare,
    release,
    dispatch,
    browserView(seat, tab) {
      if (!isOpen()) return Promise.reject(new Error('the socket is not open'));
      const { replyTo, answer } = asked(ATTACH_BOUND_MS, 'browser attach');
      sendNow({ kind: 'browser_view', seat, tab, reply_to: replyTo });
      return answer.then(attachOf);
    },
    browserTabs(seat) {
      if (!isOpen()) return Promise.reject(new Error('the socket is not open'));
      const { replyTo, answer } = asked(replyBoundMs, 'tab list');
      sendNow({ kind: 'browser_tabs', seat, reply_to: replyTo });
      return answer.then((body) => {
        if (typeof body !== 'object' || body === null) {
          throw new Error('the tab list answered nothing this page can read');
        }
        const held = body as Record<string, unknown>;
        if (held['ok'] !== true) {
          throw new Error(
            typeof held['why'] === 'string' && held['why'] !== ''
              ? held['why']
              : 'the browser could not list its tabs',
          );
        }
        return held['tabs'];
      });
    },
    browserDetach: () => sendClient({ kind: 'browser_detach' }),
    browserControl: (take) => sendClient({ kind: 'browser_control', take }),
    browserNav: (back) => sendClient({ kind: 'browser_nav', back }),
    browserResize: (width, height, scale) =>
      sendClient({ kind: 'browser_resize', width, height, scale }),
    clientPerf: (records) => sendClient({ kind: 'client_perf', records }),
    more(conversation, before = null, turns = MORE_TURNS) {
      if (!isOpen()) return false;
      sendNow({ kind: 'more', conversation, before, turns });
      return true;
    },
    commitDiff(session, sha) {
      if (!isOpen()) return false;
      sendNow({ kind: 'commit_diff', session, sha });
      return true;
    },
    fileContent(session, path, sha) {
      if (!isOpen()) return false;
      sendNow({ kind: 'file_content', session, path, sha });
      return true;
    },
    readDocument(session, path, reviewable) {
      if (!isOpen()) return false;
      sendNow({ kind: 'document_read', session, path, reviewable });
      return true;
    },
    seatRange(slot, epoch, from, to) {
      if (!isOpen()) return false;
      sendNow({ kind: 'seat_range', slot, epoch, from, to });
      return true;
    },
    frame(bytes) {
      if (!isOpen()) return false;
      try {
        // `send` takes an ArrayBufferView over an ArrayBuffer; the encoder's
        // own view is one, and TS cannot see that through the default.
        socket?.send(bytes as Uint8Array<ArrayBuffer>);
      } catch (why) {
        report('a dictation frame could not be sent', why);
        return false;
      }
      return true;
    },
    browserInput(event) {
      if (!isOpen()) return false;
      inputSeq += 1;
      sendNow({ kind: 'browser_input', seq: inputSeq, event });
      return true;
    },
    onBrowserFrame(fn) {
      frameListeners.add(fn);
      return () => frameListeners.delete(fn);
    },
    onMessage(fn) {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    onStatus(fn) {
      statuses.add(fn);
      return () => statuses.delete(fn);
    },
    store(what) {
      return stores.get(what);
    },
    settings() {
      return settings;
    },
    skew() {
      return protocolSkew;
    },
    serverProtocol() {
      return greetingProtocol;
    },
    status() {
      return status;
    },
    close() {
      move('closed');
      // A closed connection has nothing to ask on.
      bounds.stop();
      if (retry !== null) clearTimeout(retry);
      retry = null;
      failPending('the connection was closed');
      const current = socket;
      socket = null;
      current?.close();
    },
  };
}
