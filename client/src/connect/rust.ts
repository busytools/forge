/**
 * The desktop's connection: the socket lives in the client's Rust half, and
 * this is the page's window onto it.
 *
 * **Why the socket moved.** A parked webview stops consuming its socket -
 * display off, the screen locked, a page never shown, all stop it - and the
 * records it was fed stop arriving. The socket and the seat records live in
 * the Rust process now, which does not park, and this file is the one seam
 * between the halves. The browser's frames ride the Rust half's socket too:
 * the desktop overlay draws them there, and the phone hands each one to this
 * page, which paints it on a canvas.
 *
 * **The page's model is unchanged.** Every frame arrives as itself over
 * `client://inbound`, and the stores, the conversation and the dock fold it
 * exactly as they folded the websocket's own stream - including a kind this
 * build has never seen, which is drawn plainly rather than dropped.
 *
 * **The heartbeat is this side's half of the catch-up.** The Rust half counts
 * frames while the page is away - a parked page's timers stop, which is the
 * signal - and on the first heartbeat back it hands the records over as the
 * message kinds this page already reads: a snapshot per subject and the
 * newest page per seat. So a spell ends with the conversation complete, not
 * merely live again.
 *
 * The web build keeps `socket.ts`: this file is chosen at `connectTo` by
 * `canHost()`, the same marker that gates the browser surface.
 */

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

import {
  MORE_TURNS,
  skewOf,
  slotOf,
  subjectFromKey,
  subjectKey,
  type BrowserInputEvent,
  type Command,
  type ServerMessage,
  type Skew,
  type Subject,
} from '../protocol';
import { playPacket } from '../browser/audio';
import { noteSnapshot, noteSubscribeWritten, seatLabel } from '../perf';
import { READ_BOUND_MS, ReadBounds } from '../read-bound';
import { refused } from '../refusals';
import { StreamGate } from '../session/stream';
import { report, type BrowserFrame, type Connection, type ConnectionStatus } from '../socket';
import { Stores, type Store } from '../stores';
import { coversHome } from '../wire/fleet';
import { settingsFrom } from '../wire/types';
import type { ClientSettings, SessionSlot } from '../wire/types';

/**
 * How often the page heartbeats, and how long the Rust half keeps frames
 * flowing without one. This is the smaller of the two by a factor of three:
 * a slowed page (a backgrounded tab's timers run at about a tick a minute)
 * reads as away, and the reconcile that answers it is complete either way.
 */
const HEARTBEAT_MS = 5_000;

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

/** A command's variant name, thrown on when the command is not one variant. */
function variantOf(command: Command): string {
  if (typeof command === 'string') return command;
  const names = Object.keys(command);
  const [only] = names;
  if (names.length !== 1 || only === undefined) {
    throw new Error(`a command is one variant, and this one has ${names.length}`);
  }
  return only;
}

/** What the Rust half's `client_state` answers. */
interface DesktopState {
  status: ConnectionStatus;
  /** How many sockets have opened in the shell; absent on an older shell. */
  generation?: number;
  greeting: ServerMessage | null;
}

/** One `client://browser-frame` payload: a host frame, base64 pixels. */
interface ShellFrame {
  profile: number;
  tab: number;
  seq: number;
  width: number;
  height: number;
  format: number;
  key: boolean;
  page_width: number;
  page_height: number;
  bytes: string;
}

/** One `client://browser-audio` payload: interleaved i16 samples, base64. */
interface ShellAudio {
  profile: number;
  tab: number;
  seq: number;
  rate: number;
  channels: number;
  samples: string;
}

/** The frame a shell payload carries, or `null` for a shape that is not one. */
function frameOf(payload: ShellFrame): BrowserFrame | null {
  if (
    typeof payload.profile !== 'number' ||
    typeof payload.tab !== 'number' ||
    typeof payload.bytes !== 'string' ||
    typeof payload.width !== 'number' ||
    typeof payload.height !== 'number' ||
    typeof payload.page_width !== 'number' ||
    typeof payload.page_height !== 'number'
  ) {
    return null;
  }
  const binary = atob(payload.bytes);
  const bytes = new Uint8Array(binary.length);
  for (let at = 0; at < binary.length; at += 1) bytes[at] = binary.charCodeAt(at);
  return {
    profile: payload.profile,
    tab: payload.tab,
    seq: typeof payload.seq === 'number' ? payload.seq : 0,
    width: payload.width,
    height: payload.height,
    key: payload.key === true,
    page_width: payload.page_width,
    page_height: payload.page_height,
    bytes,
  };
}

/** One `client://refused` payload. */
interface DesktopRefusal {
  key: string;
  why: string;
}

/** One forwarded snapshot's frame facts, as the shell half measured them. */
interface FramedPayload {
  subject: Subject;
  bytes: number;
  parse_ms: number;
}

/** One subscribe the shell half finished writing. */
interface WrittenPayload {
  key: string;
  declared: boolean;
  waited_ms: number;
}

/**
 * The seat a subject key names, in the perf log's own spelling (#2513).
 *
 * The shell half keys a seat as `session:org\0project\0label` and every other
 * subject as its own name; the log's seat is `org/project/label`, built by
 * `seatLabel`, so this is the one other place that spelling is derived and it
 * must agree.
 */
function seatOfKey(key: string): string | null {
  const seat = key.startsWith('session:') ? key.slice('session:'.length) : null;
  if (seat === null) return null;
  const [org, project, label] = seat.split('\u0000');
  if (org === undefined || project === undefined || label === undefined) return null;
  return seatLabel({ org, project, label });
}

class RustConnection implements Connection {
  private readonly stores = new Stores();
  private readonly listeners = new Set<(message: ServerMessage) => void>();
  private readonly statuses = new Set<(status: ConnectionStatus) => void>();
  /** The page's viewers of the relayed frames: the phone's canvas. */
  private readonly frames = new Set<(frame: BrowserFrame) => void>();
  private stops: UnlistenFn[] = [];
  private beat: ReturnType<typeof setInterval> | null = null;
  private state: ConnectionStatus = 'connecting';
  /**
   * The shell's own id for this connection, from `client_connect`.
   *
   * **`close` names it**, because this page dials the next address before it
   * gives the connection it replaced back: by then the shell holds the new
   * socket, and a close that named no connection would shut that one.
   */
  private socket: number | null = null;
  /** The page gave this connection up before its dial answered. */
  private closing = false;
  private heldSettings: ClientSettings | null = null;
  private heldSkew: Skew | null = null;
  private heldProtocol: number | null = null;
  private greeted: string | null = null;
  /**
   * The seat this client has on screen, re-sent on every reconnect.
   *
   * The server's showing is per connection, and a drop ends it; `null` is
   * "nothing of mine is on screen" rather than "not attached".
   */
  private shown: SessionSlot | null = null;
  /**
   * The shell's socket generation this page last saw at an `open`, and
   * whether a status transition has been watched since.
   *
   * **A generation, not a greeting.** The shell delivers the first socket's
   * greeting twice (once on `client_state`, once as a frame) and two
   * reconnects to one server carry the same greeting value, so a greeting
   * count cannot tell a socket this page saw from one it slept through.
   * The shell counts the sockets themselves; a RISING number while this
   * page still believes the connection open is a reconnect it missed, and
   * `transition` records a drop the page really watched, so a visible
   * reconnect does not fire the pair a second time.
   */
  private generation: number | null = null;
  private transition = false;
  /**
   * The bound on a subscription's first read: a subject still loading when
   * it runs out is asked again through `client_refresh`, so a snapshot lost
   * between the server and the page is not a wait with nothing behind it.
   */
  private readonly bounds: ReadBounds;

  /**
   * The scope each subject was last asked with, for the read bound's retry.
   *
   * **The retry says nothing on the invoke** - the shell keeps the held scope
   * - but its deadline has to match the read it is retrying: an ordinary read
   * is bounded, and a review read waits for its base fetch with no cap at all,
   * so it carries no deadline.
   */
  private readonly scopes = new Map<string, boolean>();

  /** The ordinary pass this connection bounds its reads with. */
  private readonly boundMs: number;

  constructor(url: string, boundMs: number = READ_BOUND_MS) {
    this.boundMs = boundMs;
    this.bounds = new ReadBounds(this.stores, (what) => this.refreshHeld(what), boundMs);
    void this.begin(url);
  }

  /** The pass a subject's read gets, from the scope it was last asked with. */
  private passFor(what: Subject): number | null {
    return this.scopes.get(subjectKey(what)) === true ? null : this.boundMs;
  }

  /** One bound retry: no scope on the invoke, the held one's deadline. */
  private refreshHeld(what: Subject): boolean {
    if (this.state !== 'open') return false;
    this.bounds.arm(what, this.passFor(what));
    void invoke('client_refresh', { what }).catch((why: unknown) => {
      report('a refresh could not reach the shell', why);
    });
    return true;
  }

  private async begin(url: string): Promise<void> {
    // Listeners first: the greeting arrives as fast as the Rust half dials,
    // and a connection that subscribed after asking could miss it.
    this.stops.push(await listen('client://connection', () => void this.stateMoved()));
    this.stops.push(
      await listen('client://inbound', (event) => this.heard(event.payload as ServerMessage)),
    );
    // **The shell half's own facts (#2513).** A snapshot's raw length and the
    // shell's parse travel beside the frame; a subscribe's write time is
    // reported when the frame is on the wire, not when the page asked.
    this.stops.push(
      await listen<FramedPayload>('client://framed', (event) => {
        const { subject, bytes, parse_ms: parseMs } = event.payload;
        noteSnapshot(subject, bytes, parseMs);
      }),
    );
    this.stops.push(
      await listen<WrittenPayload>('client://written', (event) => {
        const { key, declared, waited_ms: waitedMs } = event.payload;
        noteSubscribeWritten(seatOfKey(key), declared, waitedMs);
      }),
    );
    this.stops.push(
      await listen<DesktopRefusal>('client://refused', (event) => this.refuse(event.payload)),
    );
    // **The phone's frames arrive here**: a platform with no native overlay
    // is handed each relayed frame as base64 and this page's canvas draws it
    // (the desktop's overlay draws its own and this listener never fires).
    this.stops.push(
      await listen<ShellFrame>('client://browser-frame', (event) => {
        const frame = frameOf(event.payload);
        if (frame === null) return;
        for (const fn of this.frames) fn(frame);
      }),
    );
    // **The phone hears the page here.** The shell hands every audio packet
    // over; the desktop plays its own and never emits these.
    this.stops.push(
      await listen<ShellAudio>('client://browser-audio', (event) => {
        const packet = event.payload;
        if (typeof packet?.samples !== 'string') return;
        playPacket(packet.samples, packet.rate, packet.channels);
      }),
    );
    // **Narrowed once at the invoke boundary**, as every wire value is: a
    // shell that answered something else leaves this connection with no id,
    // and a close with no id is not sent.
    const opened: unknown = await invoke('client_connect', { url });
    this.socket = typeof opened === 'number' ? opened : null;
    if (this.closing) {
      // A connection given up before its dial answered: it was never drawn,
      // so it is closed here rather than left retrying behind the page.
      this.end();
      return;
    }
    this.beat = setInterval(() => {
      void invoke('client_heartbeat').catch((why: unknown) => {
        // The shell is gone or restarting; the next tick tries again, and the
        // connection's own events say where it is.
        report('the heartbeat could not reach the shell', why);
      });
    }, HEARTBEAT_MS);
    await this.stateMoved();
  }

  private async stateMoved(): Promise<void> {
    const state = await invoke<DesktopState>('client_state');
    // The greeting is read by value: two reads of one greeting are two
    // objects, and the surfaces draw the skew notice off the message itself.
    const greeting = state.greeting === null ? null : JSON.stringify(state.greeting);
    if (greeting !== null && greeting !== this.greeted && state.greeting !== null) {
      this.greeted = greeting;
      this.heard(state.greeting);
    }
    const generation = typeof state.generation === 'number' ? state.generation : null;
    if (state.status === 'open' && generation !== null) {
      const known = this.generation;
      if (known === null) {
        // The first connect: nothing to compare against.
        this.generation = generation;
        this.transition = false;
      } else if (generation > known) {
        // **A socket this page never saw open.** A drop the page watched
        // fires `connecting` through its own notice, which is what
        // `transition` records; if none arrived, the webview slept across
        // the whole drop and must run the pair itself, exactly once -
        // recording the generation first makes a second queued notice of
        // the same socket a no-op.
        const missed = !this.transition;
        this.generation = generation;
        if (missed) {
          this.move('connecting');
          this.move('open');
        }
        // **Cleared AFTER the pair.** The synthesized `connecting` sets
        // `transition` as a real one would, and leaving it set would make the
        // NEXT missed reconnect read as watched: the page would record the
        // new generation and never zero `opened`, relist or re-send Showing.
        this.transition = false;
      }
    }
    this.move(state.status);
  }

  private move(next: ConnectionStatus): void {
    if (this.state === next) return;
    this.state = next;
    // **A transition this page really watched.** A drop is the only status
    // move into a non-open state, and it is what tells a socket change the
    // page saw from one it slept through; `stateMoved` clears it once the
    // generation it belongs to is recorded.
    if (next !== 'open') this.transition = true;
    // A closed connection's holds die with it: the reconnect's answer is the
    // whole record, and the numbers start wherever it says.
    if (next === 'closed') {
      this.gate.reset();
      this.bounds.stop();
    }
    // A connection that opens after a bound fired owes its loading subjects a
    // fresh deadline: the subscribe went out when the socket opened, and a
    // lost first answer would otherwise leave them loading forever. Each
    // keeps the pass its own read was asked with.
    if (next === 'open') this.bounds.rearmLoading((what) => this.passFor(what));
    // **Listeners first, then `Showing`.** The page's own re-attach runs on
    // this status and issues its `relist`; the spec's order is re-subscribe,
    // then `Showing`, and the web's `onopen` keeps the same one.
    for (const fn of this.statuses) fn(next);
    if (next === 'open' && this.shown !== null) this.sendShowing(this.shown);
  }

  private refuse(payload: DesktopRefusal): void {
    const store = this.stores.byKey(payload.key);
    if (store === undefined) {
      report(`the server refused ${payload.key}, which this client no longer holds`, payload.why);
      return;
    }
    const refused = subjectFromKey(payload.key);
    // A refusal is an answer: nothing is owed on that subject any more.
    this.bounds.clearKey(payload.key);
    store.refuse(payload.why);
    report(`the subscription to ${payload.key} was refused`, payload.why);
    if (refused === null) return;
    // **The page learns from a frame, not from the store alone.** A seat's
    // readers act on an `error` frame ("a refused subscribe is given to the
    // store it was attributed to"), and a subject store re-reads its refused
    // state on one; without this, a refused board stays on "reading the
    // board..." forever. **The frame names its subject** so a page acts on
    // its OWN refusal and not another subject's.
    this.deliver({
      kind: 'error',
      what: 'subscribe',
      why: payload.why,
      seat: typeof refused === 'object' ? refused.session : null,
      subject: refused,
    });
  }

  /**
   * One frame, into the stores and then to every listener - the same order
   * `socket.ts` keeps, so a listener that reads a store sees the frame it is
   * being told about.
   */
  /**
   * The ordering gate, and what it feeds.
   *
   * **The page folds two independent readers off this stream** - the seat
   * store and the chat - so the ordering lives ahead of both: a frame above
   * a hole is held for every reader until its repair lands (`session/stream`),
   * and the desktop gets the same rule the websocket does.
   */
  private readonly gate = new StreamGate({
    ask: (slot, epoch, from, to) => this.seatRange(slot, epoch, from, to),
    emit: (messages) => {
      for (const message of messages) this.deliver(message);
    },
    follows: (slot) => this.stores.get({ session: slot }) !== undefined,
  });

  private heard(message: ServerMessage): void {
    for (const ordered of this.gate.inbound(message)) this.deliver(ordered);
  }

  private deliver(message: ServerMessage): void {
    switch (message.kind) {
      case 'greeting': {
        this.heldSettings = settingsFrom(message.settings);
        this.heldSkew = skewOf(message);
        this.heldProtocol = typeof message.version === 'number' ? message.version : null;
        break;
      }
      case 'snapshot':
        this.bounds.clear(message.subject);
        this.stores.get(message.subject)?.set(message.data);
        break;
      case 'update': {
        const slot = slotOf(message.update);
        if (slot !== null) this.stores.get({ session: slot })?.push(message.update);
        if (coversHome(message.update)) this.stores.get('home')?.push(message.update);
        break;
      }
      case 'error':
        // A subscribe's refusal arrives as `client://refused`, which is the
        // one that has a store to land in; anything else is reported because
        // no update carries it.
        if (message.what !== 'subscribe')
          report(`the server refused a ${message.what}`, message.why);
        break;
      default:
        break;
    }
    for (const fn of this.listeners) {
      try {
        fn(message);
      } catch (why) {
        report('a message listener threw', why);
      }
    }
  }

  subscribe(what: Subject, options?: { answering?: boolean; review?: boolean }): Store {
    if (this.state === 'closed') {
      // Nothing replays a subscribe made after the connection went, so a live
      // store here would promise a snapshot that is never coming.
      return this.stores.refused(what, 'this connection is closed');
    }
    const store = this.stores.open(what);
    if (options?.review !== undefined) this.scopes.set(subjectKey(what), options.review);
    void invoke('client_subscribe', {
      what,
      answering: options?.answering ?? false,
      review: options?.review === true,
    }).catch((why: unknown) => {
      report('a subscribe could not reach the shell', why);
    });
    // The read this ask owes is bounded from here: nothing else would ask
    // again for a snapshot that simply never came. A review read is left
    // unbounded, because the server waits for its base fetch with no cap.
    this.bounds.arm(what, this.passFor(what));
    return store;
  }

  unsubscribe(what: Subject): void {
    if (this.stores.get(what) === undefined) return;
    const last = this.stores.close(what);
    if (last) this.scopes.delete(subjectKey(what));
    void invoke('client_unsubscribe', { what }).catch((why: unknown) => {
      report('an unsubscribe could not reach the shell', why);
    });
    // The gate stops sequencing a seat the page let go, same as the websocket
    // transport: its news passes through rather than opening a hole over a
    // history nobody is reading.
    if (last && typeof what !== 'string') this.gate.releaseSeat(what.session);
  }

  /**
   * Tell the server which seat is on screen.
   *
   * **Remembered and re-sent on every reconnect**: the server's showing is
   * per connection, and a drop ends it. `null` is "nothing of mine is on
   * screen", not "I am not attached".
   */
  showing(seat: SessionSlot | null): void {
    this.shown = seat;
    if (this.state === 'open') this.sendShowing(seat);
  }

  private sendShowing(seat: SessionSlot | null): void {
    void invoke('client_showing', { seat }).catch((why: unknown) => {
      report('a showing could not reach the shell', why);
    });
  }

  /**
   * Ask again for a subject whose store this client still holds.
   *
   * **A subscription's two halves die differently.** A socket drop takes the
   * server's half and leaves the store here open; asking again through
   * `subscribe` would count a second reader in the shell, and its count would
   * drift away from the server's. This re-asks with the role the seat already
   * declared.
   */
  relist(what: Subject, options?: { answering?: boolean; review?: boolean }): void {
    if (options?.review !== undefined) this.scopes.set(subjectKey(what), options.review);
    void invoke('client_relist', {
      what,
      answering: options?.answering ?? false,
      review: options?.review === true,
    }).catch((why: unknown) => {
      report('a relist could not reach the shell', why);
    });
  }

  /** The held declaration only: nothing on the wire, no store, no reader. */
  declare(what: Subject, options: { review: boolean }): void {
    // The bound's own retry reads this: a seat a page let go must not keep
    // the review read's deadline for its next ordinary ask.
    if (options.review) this.scopes.set(subjectKey(what), true);
    else this.scopes.delete(subjectKey(what));
    void invoke('client_declare', { what, review: options.review }).catch((why: unknown) => {
      report('a declaration could not reach the shell', why);
    });
  }

  /**
   * Give a subject back entirely: every store reader this page holds for it,
   * and every subscription the shell holds for it.
   *
   * **The shell owns the count**: the page may hold two readers where the
   * wire holds one, so the unsubscribe frames are the bridge's to send.
   */
  release(what: Subject): void {
    while (this.stores.get(what) !== undefined) this.stores.close(what);
    this.scopes.delete(subjectKey(what));
    void invoke('client_release', { what }).catch((why: unknown) => {
      report('a release could not reach the shell', why);
    });
    // The gate stops sequencing it: a released seat's news passes through
    // rather than opening a hole over a history nobody is reading.
    if (typeof what !== 'string') this.gate.releaseSeat(what.session);
  }

  /** `true` when the ask went, which is what a bound re-arms on. */
  refresh(what: Subject, options?: { review?: boolean }): boolean {
    // Nothing to ask when the socket is down: the reconnect re-subscribes
    // everything held, and that answer is fresher than this ask would be.
    if (this.state !== 'open') return false;
    // **A re-ask that states no scope keeps the shell's held one**, and its
    // deadline follows that held scope: a notice-driven read states nothing
    // and still resolves to the review read the seat was asked as, so it gets
    // the longer pass rather than presuming a lost read at five seconds.
    const stated = options?.review;
    const key = subjectKey(what);
    if (stated !== undefined) this.scopes.set(key, stated);
    this.bounds.arm(what, this.passFor(what));
    const payload = stated === undefined ? { what } : { what, review: stated };
    void invoke('client_refresh', payload).catch((why: unknown) => {
      report('a refresh could not reach the shell', why);
    });
    return true;
  }

  dispatch(command: Command, at?: SessionSlot): Promise<unknown> | null {
    // Before anything is registered, so a command that never went leaves no
    // promise behind for a later failure to reject.
    if (this.state !== 'open') {
      if (at !== undefined) refused(at);
      throw new Error('the socket is not open');
    }
    const reply = ANSWERS_THROUGH_A_REPLY.has(variantOf(command));
    const answer = invoke<unknown>('client_dispatch', { command, reply });
    if (!reply) {
      // Fire and forget, as the web build's non-reply path is: the outcome
      // arrives through the subscription. A refusal here - the socket went
      // between the check and the call - is reported rather than left as an
      // unhandled rejection.
      void answer.catch((why: unknown) => {
        report('a command could not cross to the shell', why);
      });
      return null;
    }
    return answer;
  }

  more(
    conversation: SessionSlot,
    before: string | null = null,
    turns: number = MORE_TURNS,
  ): boolean {
    if (this.state !== 'open') return false;
    void invoke('client_more', { conversation, before, turns }).catch((why: unknown) => {
      report('an ask for older turns could not reach the shell', why);
    });
    return true;
  }

  seatRange(slot: SessionSlot, epoch: string | null, from: number, to: number): boolean {
    if (this.state !== 'open') return false;
    void invoke('client_seat_range', { slot, epoch, from, to }).catch((why: unknown) => {
      report('a repair ask could not reach the shell', why);
    });
    return true;
  }

  commitDiff(session: SessionSlot, sha: string): boolean {
    if (this.state !== 'open') return false;
    void invoke('client_commit_diff', { session, sha }).catch((why: unknown) => {
      report("an ask for a commit's diff could not reach the shell", why);
    });
    return true;
  }

  fileContent(session: SessionSlot, path: string, sha: string | null): boolean {
    if (this.state !== 'open') return false;
    void invoke('client_file_content', { session, path, sha }).catch((why: unknown) => {
      report("an ask for a file's content could not reach the shell", why);
    });
    return true;
  }

  readDocument(session: SessionSlot, path: string, reviewable: boolean): boolean {
    if (this.state !== 'open') return false;
    void invoke('client_read_document', { session, path, reviewable }).catch((why: unknown) => {
      report('a document read could not reach the shell', why);
    });
    return true;
  }

  frame(bytes: Uint8Array): boolean {
    if (this.state !== 'open') return false;
    // A take's frames cross as a plain number array: the invoke payload is
    // JSON, so a typed array would arrive as an object and not as bytes.
    void invoke('client_frame', { bytes: Array.from(bytes) }).catch((why: unknown) => {
      report('a dictation frame could not reach the shell', why);
    });
    return true;
  }

  browserInput(event: BrowserInputEvent): boolean {
    if (this.state !== 'open') return false;
    void invoke('client_browser_input', { event }).catch((why: unknown) => {
      report('a browser gesture could not reach the shell', why);
    });
    return true;
  }

  onBrowserFrame(fn: (frame: BrowserFrame) => void): () => void {
    this.frames.add(fn);
    return () => {
      this.frames.delete(fn);
    };
  }

  onMessage(fn: (message: ServerMessage) => void): () => void {
    this.listeners.add(fn);
    return () => {
      this.listeners.delete(fn);
    };
  }

  onStatus(fn: (status: ConnectionStatus) => void): () => void {
    this.statuses.add(fn);
    return () => {
      this.statuses.delete(fn);
    };
  }

  store(what: Subject): Store | undefined {
    return this.stores.get(what);
  }

  settings(): ClientSettings | null {
    return this.heldSettings;
  }

  skew(): Skew | null {
    return this.heldSkew;
  }

  serverProtocol(): number | null {
    return this.heldProtocol;
  }

  status(): ConnectionStatus {
    return this.state;
  }

  close(): void {
    this.closing = true;
    this.move('closed');
    // A closed connection has nothing to ask on.
    this.bounds.stop();
    if (this.beat !== null) clearInterval(this.beat);
    this.beat = null;
    for (const stop of this.stops) stop();
    this.stops = [];
    this.end();
  }

  /**
   * Shut the shell's socket this page opened, and only that one.
   *
   * **Nothing before the dial answers**: the id does not exist yet, and the
   * dial's own continuation closes the socket the moment it arrives.
   */
  private end(): void {
    const opened = this.socket;
    this.socket = null;
    if (opened === null) return;
    void invoke('client_close', { id: opened }).catch(() => {
      // The shell is gone; the connection is closed here either way.
    });
  }
}

/** The desktop's connection to `url`. */
export function connectRust(url: string, boundMs: number = READ_BOUND_MS): Connection {
  return new RustConnection(url, boundMs);
}
