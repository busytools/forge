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

import { refused } from './refusals';
import {
  MORE_TURNS,
  readableProtocol,
  skewMessage,
  skewOf,
  slotOf,
  subjectKey,
  type BrowserAnswer,
  type BrowserAsk,
  type BrowserPart,
  type ClientMessage,
  type Command,
  type ServerMessage,
  type SessionUpdate,
  type Skew,
  type Subject,
} from './protocol';
import { Stores, type Store } from './stores';
import { coversHome } from './wire/fleet';
import { settingsFrom } from './wire/types';
import type { ClientSettings, SessionSlot } from './wire/types';

/** Where a connection is in its life. */
export type ConnectionStatus =
  | 'connecting'
  | 'open'
  | 'closed'
  /**
   * The server greeted with a protocol outside the range this client reads.
   * It is not a connection failure and retrying cannot fix it, so the
   * connection stops rather than reconnecting into the same answer and
   * drawing against a shape it cannot read. What the greeting said is on
   * `skew()`, so the refusal can name the builds the wire carried.
   */
  | 'mismatched';

export interface Connection {
  /**
   * Watch a subject, and get the store it is answered into.
   *
   * `answering` says whether this client can answer the prompts it is shown,
   * and it is off unless said otherwise: the core parks a turn on the reply of
   * whoever registered as answering, so a client counted as able to answer a
   * prompt it cannot display hangs the turn rather than failing it.
   *
   * Counted rather than idempotent, because the server counts it too: two
   * subscribes to one subject are two subscriptions, and one unsubscribe must
   * not take the seat out of the set the other is still watching.
   */
  subscribe(what: Subject, options?: { answering?: boolean; browser?: boolean }): Store;
  unsubscribe(what: Subject): void;
  /**
   * Ask again for a subject this connection already holds.
   *
   * A pair rather than a bare `subscribe`: the server counts a subscription
   * per subscribe and removes one per unsubscribe, and that count is what it
   * filters every update against, so a second subscribe with nothing given
   * back would grow the list for the life of the connection.
   */
  refresh(what: Subject): void;
  /**
   * Send a core command.
   *
   * Answers with a promise for the four commands whose outcome rides a reply
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
   * connection then closes, has the promise it returned rejected, because
   * the reply died with the connection and a caller waiting on a channel
   * that stays empty is worse off than one told.
   *
   * A caller that dispatches one of the four owes the returned promise an
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
   * Ask for the inputs forge can record from.
   *
   * One ask for one walk, answered by a `devices` message on `onMessage` or an
   * `error` naming it. `false` means the socket was not open and no answer is
   * coming.
   */
  devices(): boolean;
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
   * Answer the browser asks this connection is sent, as its host.
   *
   * The handler is what drives the browser: the ask arrives and it answers
   * with the tool's parts or the reason it failed, and the socket puts the
   * answer on the wire under the ask's own id. An image part's bytes ride
   * their own frame, so what a handler returns carries them.
   *
   * **Declaring `browser: true` and registering a handler travel together.**
   * A connection that declares the capability and registers nothing is sent
   * asks it answers with a failure, which reads at the far end as a session's
   * tool call failing; one that registers a handler and declares nothing is
   * never asked at all. The last registration wins, and a handler that throws
   * answers with the thrown reason rather than with silence.
   */
  onBrowserAsk(fn: (ask: BrowserAsk) => BrowserAnswer | Promise<BrowserAnswer>): () => void;
  /**
   * Whether this connection holds the browser role, as the server last said.
   *
   * The grant a capable declare is answered with, and the loss a force-take
   * sends. A strip reads this to say who drives the browser it is drawing.
   */
  browserRole(): boolean;
  /** Hear each change of the role. Answers a function that stops listening. */
  onBrowserRole(fn: (hosting: boolean) => void): () => void;
  /**
   * Take the browser role from whoever holds it.
   *
   * The claimant must have declared itself capable, which a shell with a
   * browser host does on every subscribe; the server answers with the role
   * frame either way, so a refused take is visible rather than silent.
   */
  takeBrowserRole(): void;
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
   * Set for a server one step back, which is read with the skew drawn as a
   * notice, and for one outside the range, where the status says the
   * connection stopped. Either way the surfaces name the two builds from
   * here rather than from a number they have no build for.
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
 * The commands that answer through a reply, mirroring the server's
 * `answers_through_a_reply`.
 *
 * These four are the ones with nothing behind them on the wire: a client that
 * omits `reply_to` for one of them has no second way to learn what happened.
 */
const ANSWERS_THROUGH_A_REPLY = new Set([
  'spawn_worker',
  'despawn_worker',
  'upsert_review_thread',
  'submit_review',
]);

/** How long to wait before the first re-ask, and the ceiling it doubles up to. */
const RETRY_MS = 50;
const MAX_RETRY_MS = 2000;

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

/** The kind tag a browser image frame carries, which is the wire's own. */
const BROWSER_IMAGE_TAG = 1;

/**
 * One image frame at the wire's shape: the kind tag, the answer's id as a
 * big-endian u64, then the bytes.
 *
 * Big-endian because the server reads it that way; the id is what pairs the
 * frame with the answer that declared the image, so two asks in flight at
 * once cannot be handed each other's picture.
 */
export function imageFrame(id: number, bytes: Uint8Array): Uint8Array {
  const frame = new Uint8Array(1 + 8 + bytes.length);
  frame[0] = BROWSER_IMAGE_TAG;
  new DataView(frame.buffer).setBigUint64(1, BigInt(id));
  frame.set(bytes, 9);
  return frame;
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

export function connect(url: string): Connection {
  const stores = new Stores();
  /** What to ask for again on a reconnect, in the order it was first asked. */
  const held = new Map<string, { subject: Subject; answering: boolean; browser: boolean }>();
  const listeners = new Set<(message: ServerMessage) => void>();
  const statuses = new Set<(status: ConnectionStatus) => void>();
  /**
   * The handler that drives the browser, registered by the shell that owns
   * one. `null` until then, and a connection with none answers an ask with
   * the reason rather than with nothing.
   */
  let onBrowserAsk: ((ask: BrowserAsk) => BrowserAnswer | Promise<BrowserAnswer>) | null = null;
  /**
   * Whether this connection holds the browser role, as the server last said.
   *
   * `false` until a `browser_role` frame says otherwise, because before the
   * grant the truth is "not hosting": a strip that drew "you" off the declare
   * alone would say so while another client is still the one being asked.
   */
  let browserRole = false;
  const roleListeners = new Set<(hosting: boolean) => void>();
  const pending = new Map<
    number,
    { resolve: (body: unknown) => void; reject: (why: Error) => void }
  >();
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
  let status: ConnectionStatus = 'connecting';
  let settings: ClientSettings | null = null;
  let protocolSkew: Skew | null = null;
  /** The greeting's own number, kept whether or not it skews. */
  let greetingProtocol: number | null = null;
  let nextReplyId = 1;
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
  }

  function failPending(why: string): void {
    for (const { reject } of pending.values()) reject(new Error(why));
    pending.clear();
  }

  /**
   * One update into the stores it belongs to.
   *
   * A seat's update goes to that seat's store, and to the home store when the
   * home's covering rule says a home subscriber would have been sent it -
   * which is the server's own rule, mirrored in `wire/fleet.ts` and taken
   * from there by every reader rather than decided here.
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
  function refuse(why: string): void {
    const [key] = awaiting.splice(0, 1);
    if (key === undefined) {
      report('the server refused a subscribe this client cannot account for', why);
      return;
    }
    const store = stores.byKey(key);
    if (store === undefined) {
      report(`the server refused ${key}, which this client no longer holds`, why);
      return;
    }
    store.refuse(why);
    report(`the subscription to ${key} was refused`, why);
  }

  /** One subscribe on the wire, remembered as outstanding until it is answered. */
  function askFor(what: Subject, answering: boolean, browser: boolean): void {
    sendNow({ kind: 'subscribe', what, answering, browser });
    awaiting.push(subjectKey(what));
  }

  /**
   * Re-declare the browser capability on what this connection already
   * watches.
   *
   * A take needs it: the relay registers only connections that declared, and
   * one displaced by an earlier take is no longer in its line - so the claim
   * below would be answered `false` with the click visibly doing nothing.
   * Nothing is declared when the page watches nothing yet.
   */
  function declare(): void {
    const watching = held.values().next();
    if (watching.done) return;
    askFor(watching.value.subject, watching.value.answering, true);
  }

  /** A subject the server has answered, which is no longer outstanding. */
  function answered(what: Subject): void {
    const at = awaiting.indexOf(subjectKey(what));
    if (at >= 0) awaiting.splice(at, 1);
  }

  function handle(message: ServerMessage): void {
    switch (message.kind) {
      case 'greeting': {
        settings = settingsFrom(message.settings);
        // Checked on every greeting rather than only the first: a page left
        // open across a forge upgrade reconnects to a protocol it cannot
        // read, and drawing against it silently is what this arm exists to
        // prevent. Recorded either way, because a refusal has to name the
        // server it is refusing.
        protocolSkew = skewOf(message);
        greetingProtocol = typeof message.version === 'number' ? message.version : null;
        if (protocolSkew === null) return;
        // One step back is READ rather than refused, because a floor whose
        // read is pinned by `wire/floor.test.ts` beats a client that cannot
        // draw at all; outside the range there is no read to stand on, and
        // the connection stops.
        if (readableProtocol(message.version)) return;
        report(skewMessage(protocolSkew), message);
        move('mismatched');
        socket?.close();
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
        waiting.resolve(message.body);
        return;
      }
      case 'error':
        if (message.what === 'subscribe') refuse(message.why);
        else report(`the server refused a ${message.what}`, message.why);
        return;
      // `page` is the conversation's, and `devices` is the picker's: a surface
      // that draws one reads it from `onMessage`.
      case 'page':
      case 'devices':
        return;
      // An ask is answered here rather than handed to a page: the handler is
      // the shell's, and the answer has to go back under the ask's own id.
      case 'browser_ask':
        void answerAsk(message);
        return;
      // Whether THIS connection holds the role - the grant, and the loss to a
      // force-take. Kept here rather than on a page: it is the connection's
      // own fact, and every strip reads it from one place.
      case 'browser_role':
        browserRole = message.hosting;
        for (const hear of roleListeners) hear(browserRole);
        return;
      // A frame whose kind this client does not know is REPORTED rather than
      // dropped in silence: a message that arrived and drew nothing is
      // indistinguishable from one that never arrived.
      default:
        report('the server sent a frame this client does not know', message);
        return;
    }
  }

  /**
   * One ask answered and put back on the wire.
   *
   * Every path answers: a handler that throws, and a connection with no
   * handler at all, both send the failed shape naming why. A session's tool
   * call is waiting on this, so silence here is a turn that hangs.
   */
  async function answerAsk(ask: BrowserAsk): Promise<void> {
    let answer: BrowserAnswer;
    try {
      answer =
        onBrowserAsk === null
          ? { error: 'this client cannot host the browser' }
          : await onBrowserAsk(ask);
    } catch (why) {
      answer = { error: `the browser handler failed: ${String(why)}` };
    }
    // The connection the ask arrived on is gone: there is nothing to send the
    // answer down, and the server frees the role with the connection.
    if (!isOpen()) {
      report('a browser ask was answered after the socket closed', ask);
      return;
    }
    if ('error' in answer) {
      sendNow({ kind: 'browser_answer', id: ask.id, parts: [], error: answer.error });
      return;
    }
    // The mime types cross on the answer; the bytes follow as frames, in the
    // order the parts are listed. **The answer goes first**, which is what the
    // server reads: it declares the images before any frame can fill them.
    const parts: BrowserPart[] = answer.parts.map((part) =>
      part.type === 'image' ? { type: 'image', mime_type: part.mime_type } : part,
    );
    sendNow({ kind: 'browser_answer', id: ask.id, parts, error: null });
    for (const part of answer.parts) {
      if (part.type !== 'image') continue;
      try {
        // `send` takes an ArrayBufferView over an ArrayBuffer; the frame's own
        // view is one, and TS cannot see that through the default.
        socket?.send(imageFrame(ask.id, part.bytes) as Uint8Array<ArrayBuffer>);
      } catch (why) {
        // **A frame that could not be sent cannot be skipped.** The answer is
        // already on the wire and its images will never all arrive, so the
        // tool call would wait on a promise this client cannot keep. Dropping
        // the connection ends that ask - the server fails it naming the host
        // that went away - and the reconnect re-declares the capability.
        report('an image frame could not be sent; dropping the connection', why);
        socket?.close();
        return;
      }
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
    socket = next;

    next.onopen = () => {
      if (next !== socket) return;
      move('open');
      retryDelay = RETRY_MS;
      for (const { subject, answering, browser } of held.values()) {
        // Once per subscription, which is how many times the server was asked
        // before the drop: one unsubscribe drops one of its entries, so
        // re-asking once for a subject held twice would leave its count lower
        // than this side's, and a later unsubscribe would take the subject
        // away from a caller still drawing it.
        for (let remaining = stores.count(subject); remaining > 0; remaining -= 1) {
          askFor(subject, answering, browser);
        }
      }
    };

    next.onmessage = (event) => {
      if (next !== socket) return;
      let message: ServerMessage;
      try {
        // The boundary: a frame is narrowed once, here, rather than by each
        // reader. A frame this client cannot read is reported rather than
        // thrown, because a throw out of this handler would take every frame
        // after it as well.
        message = JSON.parse(String(event.data)) as ServerMessage;
      } catch (why) {
        report('the server sent a frame this client could not read', why);
        return;
      }
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

    next.onclose = () => {
      if (next !== socket) return;
      socket = null;
      // **The role dies with the connection it belonged to.** The relay keeps
      // the role for the CONNECTION id, not for the page, so a reconnected
      // client is a new one - and a strip that went on saying "this client
      // drives the browser" through a drop would hide the override that is
      // the only way back. Reset before the drop guard, so even the final
      // close says the truth.
      if (browserRole) {
        browserRole = false;
        for (const hear of roleListeners) hear(browserRole);
      }
      // A mismatched protocol is not something a retry answers, so the close
      // that follows it must not be read as a drop.
      if (status === 'closed' || status === 'mismatched') return;
      move('connecting');
      // A command that was in flight has no answer coming: the reply died
      // with the connection that would have carried it. So did every ask -
      // and `onopen` asks again, so leaving them queued would put stale keys
      // in front of the live ones and hand a later refusal to a store the
      // server had already answered.
      failPending('the socket dropped before answering');
      awaiting.length = 0;
      scheduleRetry();
    };

    // An error is always followed by a close, which is where the retry is
    // scheduled - this reports it, because a socket that keeps failing is
    // otherwise indistinguishable from one that is merely slow.
    next.onerror = (event) => report(`the socket to ${url} failed`, event);
  }

  function scheduleRetry(): void {
    retry = setTimeout(open, retryDelay);
    retryDelay = Math.min(retryDelay * 2, MAX_RETRY_MS);
  }

  function subscribe(what: Subject, options?: { answering?: boolean; browser?: boolean }): Store {
    if (status === 'closed') {
      // Nothing replays a subscribe made after the socket went, so a live
      // store here would promise a snapshot that is never coming.
      return stores.refused(what, 'this connection is closed');
    }

    const answering = options?.answering ?? false;
    const browser = options?.browser ?? false;
    const key = subjectKey(what);
    const store = stores.open(what);

    const existing = held.get(key);
    if (existing === undefined) {
      held.set(key, { subject: what, answering, browser });
    } else {
      // The core's stream only ever escalates, so a later answering
      // subscribe raises the role the reconnect re-declares. The browser
      // capability is the same: a connection that has declared it does not
      // un-declare it by subscribing again without it.
      if (answering) existing.answering = true;
      if (browser) existing.browser = true;
    }

    if (isOpen()) {
      const declaration = held.get(key);
      askFor(what, declaration?.answering ?? answering, declaration?.browser ?? browser);
    }
    return store;
  }

  function unsubscribe(what: Subject): void {
    if (stores.get(what) === undefined) return;
    const last = stores.close(what);
    if (isOpen()) sendNow({ kind: 'unsubscribe', what });
    // Dropped only with the last one, so a reconnect re-asks for what is
    // still being watched and not the seat one caller walked away from.
    if (last) held.delete(subjectKey(what));
  }

  function refresh(what: Subject): void {
    // Nothing to ask when the socket is down: the reconnect re-subscribes
    // everything held, and that answer is fresher than this ask would be.
    if (!isOpen()) return;
    sendNow({ kind: 'unsubscribe', what });
    const declaration = held.get(subjectKey(what));
    askFor(what, declaration?.answering ?? false, declaration?.browser ?? false);
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

    const replyTo = nextReplyId;
    nextReplyId += 1;
    // Registered before it goes, because a refusal can come back as fast as
    // the send returns.
    const answer = new Promise<unknown>((resolve, reject) => {
      pending.set(replyTo, { resolve, reject });
    });
    try {
      sendNow({ kind: 'command', command, reply_to: replyTo });
    } catch (why) {
      // `JSON.stringify` is the one thing between the registration and the
      // send that can throw - a BigInt in a command - and an entry left
      // behind would be rejected later as though the command had gone.
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
    dispatch,
    more(conversation, before = null, turns = MORE_TURNS) {
      if (!isOpen()) return false;
      sendNow({ kind: 'more', conversation, before, turns });
      return true;
    },
    devices() {
      if (!isOpen()) return false;
      sendNow({ kind: 'devices' });
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
    onBrowserAsk(fn) {
      onBrowserAsk = fn;
      return () => {
        // Only the registration that is still in place is cleared: a listener
        // that unregisters after another took over must not take the live one
        // with it.
        if (onBrowserAsk === fn) onBrowserAsk = null;
      };
    },
    browserRole() {
      return browserRole;
    },
    onBrowserRole(fn) {
      roleListeners.add(fn);
      return () => roleListeners.delete(fn);
    },
    takeBrowserRole() {
      // A click while the socket is down is dropped with a record rather than
      // thrown into the click handler: a command that cannot cross is the
      // connection's state, not a page error.
      if (!isOpen()) {
        report('the browser role could not be claimed', 'the socket is not open');
        return;
      }
      // The claim re-declares first: the relay holds a channel only for a
      // connection that declared, and a connection displaced by an earlier
      // take is no longer registered - so the bare claim would come back
      // `false` with nothing re-registered. The declare puts this connection
      // back in the line the claim moves; the watch entry and hold it adds
      // are balanced when the page leaves.
      declare();
      sendNow({ kind: 'browser_take_role' });
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
      if (retry !== null) clearTimeout(retry);
      retry = null;
      failPending('the connection was closed');
      const current = socket;
      socket = null;
      current?.close();
    },
  };
}
