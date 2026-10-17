/**
 * The desktop's connection: the socket lives in the client's Rust half, and
 * this is the page's window onto it.
 *
 * **Why the socket moved.** A parked webview stops consuming its socket -
 * display off, the screen locked, a page never shown, all stop it - and a
 * `browser_ask` frame that dies in delivery parks the session's tool call
 * with it (the 2026-10-10 catch: twelve asks at the 200s bound across two
 * seats). The socket, the asks and the seat records live in the Rust process
 * now, which does not park, and this file is the one seam between the halves.
 *
 * **The page's model is unchanged.** Every frame arrives as itself over
 * `client://inbound`, and the stores, the conversation and the dock fold it
 * exactly as they folded the websocket's own stream - including a kind this
 * build has never seen, which is drawn plainly rather than dropped. What the
 * shim adds is the connection's own facts, which never rode the frames: where
 * it is in its life, who holds the browser role, and the ask ring.
 *
 * **The heartbeat is this side's half of the catch-up.** The Rust half counts
 * frames while the page is away - a parked page's timers stop, which is the
 * signal - and on the first heartbeat back it hands the records over as the
 * message kinds this page already reads: a snapshot per subject and the
 * newest page per seat. So a spell ends with the conversation complete, not
 * merely live again.
 *
 * The web build keeps `socket.ts`: this file is chosen at `connectTo` by
 * `canHost()`, the same marker that gates the browser role.
 */

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

import { callDown, callUp } from '../browser/inflight.svelte';
import {
  MORE_TURNS,
  skewOf,
  slotOf,
  type BrowserAnswer,
  type BrowserAsk,
  type Command,
  type ServerMessage,
  type Skew,
  type Subject,
} from '../protocol';
import { READ_BOUND_MS, ReadBounds } from '../read-bound';
import { refused } from '../refusals';
import { StreamGate } from '../session/stream';
import { report, type Connection, type ConnectionStatus } from '../socket';
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
 * The commands that answer through a reply, mirroring the server's own set.
 * The web build's `socket.ts` keeps the same list; which of the two a command
 * is stays this side's decision, and the Rust half is told.
 */
const ANSWERS_THROUGH_A_REPLY = new Set([
  'spawn_worker',
  'despawn_worker',
  'upsert_review_thread',
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
  greeting: ServerMessage | null;
  role: boolean;
}

/** One `client://refused` payload. */
interface DesktopRefusal {
  key: string;
  why: string;
}

class RustConnection implements Connection {
  private readonly stores = new Stores();
  private readonly listeners = new Set<(message: ServerMessage) => void>();
  private readonly statuses = new Set<(status: ConnectionStatus) => void>();
  private readonly roles = new Set<(hosting: boolean) => void>();
  /** The strip's re-reads after the host swept its profiles: the sweep runs
   *  in the Rust half, and this is how a page learns its rows moved. */
  private readonly swept = new Set<() => void>();
  /**
   * The ask handler, kept for the interface's own sake: on the desktop no
   * ask reaches this page at all - the Rust half answers them - so nothing
   * ever calls this. `hostTheBrowser` is not the desktop's path.
   */
  private handler: ((ask: BrowserAsk) => BrowserAnswer | Promise<BrowserAnswer>) | null = null;
  private stops: UnlistenFn[] = [];
  private beat: ReturnType<typeof setInterval> | null = null;
  private state: ConnectionStatus = 'connecting';
  private hosting = false;
  private heldSettings: ClientSettings | null = null;
  private heldSkew: Skew | null = null;
  private heldProtocol: number | null = null;
  private greeted: string | null = null;
  private asks = 0;
  /**
   * The bound on a subscription's first read: a subject still loading when
   * it runs out is asked again through `client_refresh`, so a snapshot lost
   * between the server and the page is not a wait with nothing behind it.
   */
  private readonly bounds: ReadBounds;

  constructor(url: string, boundMs: number = READ_BOUND_MS) {
    this.bounds = new ReadBounds(this.stores, (what) => this.refresh(what), boundMs);
    void this.begin(url);
  }

  private async begin(url: string): Promise<void> {
    // Listeners first: the greeting arrives as fast as the Rust half dials,
    // and a connection that subscribed after asking could miss it.
    this.stops.push(await listen('client://connection', () => void this.stateMoved()));
    this.stops.push(
      await listen('client://inbound', (event) => this.heard(event.payload as ServerMessage)),
    );
    this.stops.push(
      await listen<DesktopRefusal>('client://refused', (event) => this.refuse(event.payload)),
    );
    this.stops.push(
      await listen<boolean>('client://role', (event) => this.roleMoved(event.payload)),
    );
    this.stops.push(
      await listen<number>('client://asks', (event) => this.asksMoved(event.payload)),
    );
    this.stops.push(
      await listen('client://profiles', () => {
        for (const hear of this.swept) hear();
      }),
    );
    await invoke('client_connect', { url });
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
    this.roleMoved(state.role);
    // The greeting is read by value: two reads of one greeting are two
    // objects, and the surfaces draw the skew notice off the message itself.
    const greeting = state.greeting === null ? null : JSON.stringify(state.greeting);
    if (greeting !== null && greeting !== this.greeted && state.greeting !== null) {
      this.greeted = greeting;
      this.heard(state.greeting);
    }
    this.move(state.status);
  }

  private move(next: ConnectionStatus): void {
    if (this.state === next) return;
    this.state = next;
    // A closed connection's holds die with it: the reconnect's answer is the
    // whole record, and the numbers start wherever it says.
    if (next === 'closed') this.gate.reset();
    for (const fn of this.statuses) fn(next);
  }

  private roleMoved(hosting: boolean): void {
    if (this.hosting === hosting) return;
    this.hosting = hosting;
    for (const fn of this.roles) fn(hosting);
  }

  /** The strip's ring, fed from the Rust half's own count of asks. */
  private asksMoved(inflight: number): void {
    while (this.asks < inflight) {
      this.asks += 1;
      callUp();
    }
    while (this.asks > inflight) {
      this.asks -= 1;
      callDown();
    }
  }

  private refuse(payload: DesktopRefusal): void {
    const store = this.stores.byKey(payload.key);
    if (store === undefined) {
      report(`the server refused ${payload.key}, which this client no longer holds`, payload.why);
      return;
    }
    // A refusal is an answer: nothing is owed on that subject any more.
    this.bounds.clearKey(payload.key);
    store.refuse(payload.why);
    report(`the subscription to ${payload.key} was refused`, payload.why);
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

  subscribe(what: Subject, options?: { answering?: boolean; browser?: boolean }): Store {
    if (this.state === 'closed') {
      // Nothing replays a subscribe made after the connection went, so a live
      // store here would promise a snapshot that is never coming.
      return this.stores.refused(what, 'this connection is closed');
    }
    const store = this.stores.open(what);
    void invoke('client_subscribe', {
      what,
      answering: options?.answering ?? false,
      browser: options?.browser ?? false,
    }).catch((why: unknown) => {
      report('a subscribe could not reach the shell', why);
    });
    // The read this ask owes is bounded from here: nothing else would ask
    // again for a snapshot that simply never came.
    this.bounds.arm(what);
    return store;
  }

  unsubscribe(what: Subject): void {
    if (this.stores.get(what) === undefined) return;
    const last = this.stores.close(what);
    void invoke('client_unsubscribe', { what }).catch((why: unknown) => {
      report('an unsubscribe could not reach the shell', why);
    });
    // The gate stops sequencing a seat the page let go, same as the websocket
    // transport: its news passes through rather than opening a hole over a
    // history nobody is reading.
    if (last && typeof what !== 'string') this.gate.releaseSeat(what.session);
  }

  /** `true` when the ask went, which is what a bound re-arms on. */
  refresh(what: Subject): boolean {
    // Nothing to ask when the socket is down: the reconnect re-subscribes
    // everything held, and that answer is fresher than this ask would be.
    if (this.state !== 'open') return false;
    this.bounds.arm(what);
    void invoke('client_refresh', { what }).catch((why: unknown) => {
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

  onBrowserAsk(fn: (ask: BrowserAsk) => BrowserAnswer | Promise<BrowserAnswer>): () => void {
    this.handler = fn;
    return () => {
      if (this.handler === fn) this.handler = null;
    };
  }

  browserRole(): boolean {
    return this.hosting;
  }

  onBrowserRole(fn: (hosting: boolean) => void): () => void {
    this.roles.add(fn);
    return () => {
      this.roles.delete(fn);
    };
  }

  onProfilesChanged(fn: () => void): () => void {
    this.swept.add(fn);
    return () => {
      this.swept.delete(fn);
    };
  }

  takeBrowserRole(): void {
    if (this.state !== 'open') {
      report('the browser role could not be claimed', 'the socket is not open');
      return;
    }
    void invoke('client_take_role').catch((why: unknown) => {
      report('the browser role could not be claimed', why);
    });
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
    this.move('closed');
    // A closed connection has nothing to ask on.
    this.bounds.stop();
    if (this.beat !== null) clearInterval(this.beat);
    this.beat = null;
    for (const stop of this.stops) stop();
    this.stops = [];
    void invoke('client_close').catch(() => {
      // The shell is gone; the connection is closed here either way.
    });
  }
}

/** The desktop's connection to `url`. */
export function connectRust(url: string, boundMs: number = READ_BOUND_MS): Connection {
  return new RustConnection(url, boundMs);
}
