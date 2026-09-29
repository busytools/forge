/**
 * The connection to a running forge: one socket, the stores it answers into,
 * and the reconnect that puts it back.
 *
 * **The socket carries no history.** A reconnect is a new connection - the
 * server's registry of who is watching what lived on the old one - so
 * everything held here is asked for again, and the stores keep what they had
 * until the fresh answer arrives. Blanking them first would show the reader a
 * page emptying and refilling on every blip.
 */

import {
  MORE_TURNS,
  slotOf,
  subjectKey,
  type ClientMessage,
  type Command,
  type ServerMessage,
  type SessionUpdate,
  type Subject,
} from './protocol';
import { Stores, type Store } from './stores';
import type { ClientSettings, SessionSlot } from './wire/types';

/** Where a connection is in its life. */
export type ConnectionStatus = 'connecting' | 'open' | 'closed';

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
  subscribe(what: Subject, options?: { answering?: boolean }): Store;
  unsubscribe(what: Subject): void;
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
   * A caller that dispatches one of those four owes the promise an answer:
   * nothing else carries the outcome, and a connection that closes rejects
   * what it was still holding rather than leaving it to hang.
   */
  dispatch(command: Command): Promise<unknown> | null;
  /**
   * Ask for older turns of one conversation.
   *
   * A page arrives at `onMessage` rather than at a promise: `Page` carries no
   * id to pair it with its ask, so two asks for one conversation could not be
   * told apart, and asking twice at once is ordinary rather than a mistake.
   */
  more(conversation: SessionSlot, before?: string | null, turns?: number): void;
  /** Every message the server sent, unparsed by anything here. Answers a function that stops listening. */
  onMessage(fn: (message: ServerMessage) => void): () => void;
  store(what: Subject): Store | undefined;
  /** The client's mark, theme and font, from the greeting - the only place a client gets them. */
  settings(): ClientSettings | null;
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

/** A command's variant name, which is the only key an externally-tagged command has. */
function variantOf(command: Command): string | null {
  return Object.keys(command)[0] ?? null;
}

export function connect(url: string): Connection {
  const stores = new Stores();
  /** What to ask for again on a reconnect, in the order it was first asked. */
  const held = new Map<string, { subject: Subject; answering: boolean }>();
  const listeners = new Set<(message: ServerMessage) => void>();
  const pending = new Map<
    number,
    { resolve: (body: unknown) => void; reject: (why: Error) => void }
  >();

  let socket: WebSocket | null = null;
  let status: ConnectionStatus = 'connecting';
  let settings: ClientSettings | null = null;
  let nextReplyId = 1;
  let retry: ReturnType<typeof setTimeout> | null = null;
  let retryDelay = RETRY_MS;

  function sendNow(message: ClientMessage): void {
    if (socket === null || socket.readyState !== WebSocket.OPEN) {
      throw new Error('the socket is not open');
    }
    socket.send(JSON.stringify(message));
  }

  function failPending(why: string): void {
    for (const { reject } of pending.values()) reject(new Error(why));
    pending.clear();
  }

  /**
   * One update into the stores it belongs to.
   *
   * A seat's updates go to that seat's store. They also go to the home store,
   * which over-includes: the server sends an update when ANY watched subject
   * covers it, and which of them that was is not on the wire. A slot-less
   * update is only ever covered by the home subscription, so that half is
   * exact; a seat's is covered by home only when the server's `fleet_news`
   * classifies it as news to the fleet, which is a filter this layer would
   * have to keep a second copy of. Until the home page brings that
   * classification with it, the home store takes the wider set and the page
   * draws from the part it knows.
   */
  function route(update: SessionUpdate): void {
    const slot = slotOf(update);
    if (slot !== null) stores.get({ session: slot })?.push(update);
    stores.get('home')?.push(update);
  }

  function handle(message: ServerMessage): void {
    switch (message.kind) {
      case 'greeting':
        settings = message.settings;
        return;
      case 'snapshot':
        stores.get(message.subject)?.set(message.data);
        return;
      case 'update':
        route(message.update);
        return;
      case 'reply': {
        const waiting = pending.get(message.reply_to);
        if (waiting === undefined) return;
        pending.delete(message.reply_to);
        waiting.resolve(message.body);
        return;
      }
      // `page` is the conversation's and `error` is whoever asked's, and
      // neither has an owner in this layer: both reach `onMessage`.
      case 'page':
      case 'error':
        return;
    }
  }

  function open(): void {
    retry = null;
    if (status === 'closed') return;

    const next = new WebSocket(url);
    socket = next;

    next.onopen = () => {
      if (next !== socket) return;
      status = 'open';
      retryDelay = RETRY_MS;
      for (const { subject, answering } of held.values()) {
        next.send(JSON.stringify({ kind: 'subscribe', what: subject, answering }));
      }
    };

    next.onmessage = (event) => {
      if (next !== socket) return;
      const text = String(event.data);
      const message = JSON.parse(text) as ServerMessage;
      handle(message);
      for (const fn of listeners) fn(message);
    };

    next.onclose = () => {
      if (next !== socket) return;
      socket = null;
      if (status === 'closed') return;
      status = 'connecting';
      // A command that was in flight has no answer coming: the reply died
      // with the connection that would have carried it.
      failPending('the socket dropped before answering');
      retry = setTimeout(open, retryDelay);
      retryDelay = Math.min(retryDelay * 2, MAX_RETRY_MS);
    };

    // An error is always followed by a close, which is where the retry is
    // scheduled - this only keeps the event from being unhandled.
    next.onerror = () => undefined;
  }

  function subscribe(what: Subject, options?: { answering?: boolean }): Store {
    const answering = options?.answering ?? false;
    const key = subjectKey(what);
    const store = stores.open(what);

    const existing = held.get(key);
    if (existing === undefined) {
      held.set(key, { subject: what, answering });
    } else if (answering) {
      // The core's stream only ever escalates, so a later answering
      // subscribe raises the role the reconnect re-declares.
      existing.answering = true;
    }

    if (status === 'open') {
      sendNow({ kind: 'subscribe', what, answering: held.get(key)?.answering ?? answering });
    }
    return store;
  }

  function unsubscribe(what: Subject): void {
    if (stores.get(what) === undefined) return;
    const last = stores.close(what);
    if (status === 'open') sendNow({ kind: 'unsubscribe', what });
    // Dropped only with the last one, so a reconnect re-asks for what is
    // still being watched and not the seat one caller walked away from.
    if (last) held.delete(subjectKey(what));
  }

  function dispatch(command: Command): Promise<unknown> | null {
    const variant = variantOf(command);
    if (variant === null || !ANSWERS_THROUGH_A_REPLY.has(variant)) {
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
    sendNow({ kind: 'command', command, reply_to: replyTo });
    return answer;
  }

  open();

  return {
    subscribe,
    unsubscribe,
    dispatch,
    more(conversation, before = null, turns = MORE_TURNS) {
      sendNow({ kind: 'more', conversation, before, turns });
    },
    onMessage(fn) {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    store(what) {
      return stores.get(what);
    },
    settings() {
      return settings;
    },
    status() {
      return status;
    },
    close() {
      status = 'closed';
      if (retry !== null) clearTimeout(retry);
      retry = null;
      failPending('the connection was closed');
      const current = socket;
      socket = null;
      current?.close();
    },
  };
}
