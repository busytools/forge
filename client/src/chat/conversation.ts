/**
 * The conversation the chat draws: whole turns, held newest last, plus the
 * handle that asks for the ones above them.
 *
 * **A turn is a RUN of the conversation, and the server decides where it
 * breaks.** `more` asks for a number of turns and answers with whole ones, so
 * a page can never land inside a turn and the client never has to cut one. It
 * does not follow that a client holds the whole conversation: it holds what it
 * has paged in and asks for more when the reader reaches the top.
 *
 * **A row's key is the turn's own, assigned once and never recomputed.** A
 * key derived at read time would change when older turns are prepended - and
 * `virtua` stores its measured sizes PER KEY, so a key that shifts throws away
 * what it learned and re-measures the list, taking the reader's place with it.
 * That is why a turn the fold could not name is named here at ingest, from its
 * own content, rather than being derived by whoever draws it.
 *
 * **A page may repeat a row the client already holds.** The server errs toward
 * repeating rather than toward a gap, because a turn the client was never sent
 * cannot be asked for again - so the repeats are dropped by key on the way in
 * and the rows already drawn keep the objects they were.
 */

import { get, writable, type Readable } from 'svelte/store';

import { MORE_TURNS, slotOf } from '../protocol';
import type { ServerMessage, SessionUpdate } from '../protocol';
import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';

/** One turn as a page carries it: the fold's name, and the CLI's messages. */
export interface PageTurn {
  /** `null` for a turn the fold could not name, which is what the chat names. */
  key: string | null;
  messages: unknown[];
}

/** One turn as the conversation holds it. The key is never null: it is named on the way in. */
export interface Turn {
  key: string;
  messages: unknown[];
  /**
   * A turn the FRAMES built rather than a page.
   *
   * It is replaced wholesale when the server's own fold of that turn arrives,
   * so it is dropped on the first page after it: keeping both would draw the
   * turn twice, once as the frames had it and once as the fold settled it.
   */
  live: boolean;
}

/** What the chat draws from. */
export interface Conversation {
  /** Whole turns, oldest first: the order the list draws them in. */
  turns: Turn[];
  /** Whether the first page has landed, which is what tells loading from empty. */
  loaded: boolean;
  /** The handle the page carried for the turns above it, or `null` at the top. */
  cursor: string | null;
  /** The server's own words when it turned the conversation down. */
  refused: string | null;
  /** Whether the reader is at the newest end, which is what decides whether it follows. */
  atEnd: boolean;
  /**
   * How many pages of OLDER turns have landed.
   *
   * A count rather than a flag because the list has to know when one change
   * differs from the last, and a flag that goes back to `false` reads the same
   * twice. It is what tells the list to compensate for a prepend and not for
   * anything else: the compensation is written for rows arriving ABOVE the
   * reader, and applying it to a turn appended below them moves them by that
   * row's height, which is the one thing this page must not do.
   */
  prepends: number;
}

/** A conversation nothing has answered yet. */
export const NOTHING: Conversation = {
  turns: [],
  loaded: false,
  cursor: null,
  refused: null,
  atEnd: true,
  prepends: 0,
};

/**
 * What names a turn the fold did not.
 *
 * A digest of the turn's own messages, which is stable under everything that
 * happens above and below it - a prepend changes no held turn's content, and
 * an append lands in a turn of its own. It is not a hash for safety: the worst
 * a collision does is hand two turns one name, which is what the ordinal in
 * `nameIn` is for.
 */
function digest(messages: unknown[]): string {
  const text = JSON.stringify(messages) ?? '';
  // FNV-1a, 32 bits: small, deterministic, and this is a name rather than a
  // fingerprint - nothing is protected by it.
  let hash = 0x811c9dc5;
  for (let at = 0; at < text.length; at += 1) {
    hash ^= text.charCodeAt(at);
    hash = Math.imul(hash, 0x01000193);
  }
  return `turn-${(hash >>> 0).toString(16)}`;
}

/**
 * A name for one turn, given the names already taken.
 *
 * The fold's own name where it gave one. A name already held takes an ordinal
 * rather than displacing the row that has it: keys are assigned once and never
 * revisited, so a page repeating a turn cannot rename the one the reader is
 * looking at.
 */
function nameIn(turn: PageTurn, taken: ReadonlySet<string>): string {
  const base = turn.key ?? digest(turn.messages);
  if (!taken.has(base)) return base;
  for (let nth = 2; ; nth += 1) {
    const candidate = `${base}#${nth}`;
    if (!taken.has(candidate)) return candidate;
  }
}

/**
 * A live turn's name, from the frame that opened it.
 *
 * The frame's own id where it carries one. A turn still being written has no
 * settled content to digest, and digesting what it has would rename it on
 * every frame - so its name has to come from something that does not grow. The
 * ordinal is the fallback for a frame with no id at all, and it is read once:
 * a turn named `live-3` keeps that name even after older turns are prepended
 * under it.
 */
function liveName(message: unknown, at: number): string {
  const id = (message as { uuid?: unknown } | null)?.uuid;
  return typeof id === 'string' && id !== '' ? `live:${id}` : `live:${at}`;
}

/** The messages of one frame, which is what a turn's own content is. */
function messagesOf(turn: PageTurn): unknown[] {
  return Array.isArray(turn.messages) ? turn.messages : [];
}

/** Whether a frame opens a turn of its own: what a person said, or a delivery. */
function opensATurn(message: unknown): boolean {
  const type = (message as { type?: unknown } | null)?.type;
  return type === 'user' || type === 'system';
}

/** The update's variant name, for the ones the chat acts on. */
function variantOf(update: SessionUpdate): string | null {
  if (typeof update === 'string') return update;
  const [name] = Object.keys(update);
  return name ?? null;
}

/**
 * One conversation, over one connection.
 *
 * It asks for a page, holds what comes back and follows the seat's frames.
 * `start` is what puts it to work and `stop` is what takes it off again, so a
 * page that unmounts leaves neither a subscription nor a listener behind.
 */
export class Chat {
  private readonly connection: Connection;
  private readonly slot: SessionSlot;
  private readonly inner = writable<Conversation>(NOTHING);
  /**
   * The page being waited on, which is both the guard against a second ask
   * queueing behind it and the direction the answer goes: a page asked for by
   * cursor belongs ABOVE what is held, and one asked for without belongs at
   * the end of it.
   */
  private inFlight: 'newest' | 'older' | null = null;
  /** What `start` has to undo, and `null` while the chat is stopped. */
  private running: (() => void) | null = null;

  constructor(connection: Connection, slot: SessionSlot) {
    this.connection = connection;
    this.slot = slot;
  }

  /** The conversation, for a component to draw. */
  get value(): Readable<Conversation> {
    return { subscribe: this.inner.subscribe };
  }

  /**
   * Listen to the seat and ask for its newest page.
   *
   * **It listens rather than subscribes.** The session page already holds the
   * seat's subscription, and a second one would be a second full encode of
   * the session per reconnect without a second listener's worth of news: the
   * frames this needs - the turn's messages, and the update that says it
   * settled - arrive on the connection either way.
   *
   * **And a subscription is not what fills the list.** The snapshot carries
   * the conversation in full, which is the whole transcript; the page draws a
   * window of it and asks `more` for what is above, so the ask is where its
   * turns come from.
   */
  start(): () => void {
    if (this.running !== null) return this.running;
    const stopMessages = this.connection.onMessage((message: ServerMessage) =>
      this.receive(message),
    );
    // A page is a question asked over one connection, and a dropped socket
    // takes the answer with it - nothing replays a `more`. So the reconnect is
    // where the conversation is asked for again, and the reader keeps what
    // they have until the fresh answer lands.
    const stopStatus = this.connection.onStatus((status) => {
      if (status === 'open') this.ask(null);
    });
    this.running = () => {
      stopMessages();
      stopStatus();
      this.running = null;
    };
    this.ask(null);
    return this.running;
  }

  /**
   * Ask for the turns above the oldest held one, answering whether it asked.
   *
   * The answer is what the list turns its compensation on with, and it has to
   * be on BEFORE the page lands: the compensation is applied as the rows
   * change, so a caller that switched it on when the answer arrived would be
   * switching it on after the change it exists for.
   */
  older(): boolean {
    const { cursor, loaded } = this.read();
    // Nothing above the page already drawn is the server's own `null`, and
    // asking again on it would walk the same page forever.
    if (!loaded || cursor === null || this.inFlight !== null) return false;
    this.ask(cursor);
    return true;
  }

  /** Ask for the newest page again, which is what replaces a settled turn. */
  refresh(): void {
    this.ask(null);
  }

  /** Where the reader is, which decides whether the newest turn is followed. */
  position(atEnd: boolean): void {
    this.inner.update((held) => (held.atEnd === atEnd ? held : { ...held, atEnd }));
  }

  private read(): Conversation {
    return get(this.inner);
  }

  private ask(before: string | null): void {
    if (this.inFlight !== null) return;
    this.inFlight = before === null ? 'newest' : 'older';
    // A closed socket answers nothing, so the flag must not stay set waiting
    // on a page that was never asked for.
    if (!this.connection.more(this.slot, before, MORE_TURNS)) this.inFlight = null;
  }

  private receive(message: ServerMessage): void {
    switch (message.kind) {
      case 'page':
        if (sameSlot(message.conversation, this.slot)) {
          this.takePage(message.turns, message.cursor, this.inFlight ?? 'newest');
        }
        return;
      case 'error':
        // A refusal names what failed rather than which subject, and the
        // socket hands a listener EVERY message it receives - so a page that
        // took any error as its own would draw a refused subscription, or a
        // refused command, as a conversation this forge will not answer for.
        if (message.what !== 'more') return;
        this.inner.update((held) => ({ ...held, refused: message.why, loaded: true }));
        return;
      case 'update':
        this.takeUpdate(message.update);
        return;
      default:
        return;
    }
  }

  /**
   * A page of whole turns, folded into what is held.
   *
   * A turn the page repeats keeps the OBJECT it was, so a row the reader is
   * looking at is not re-rendered by an answer that says nothing new:
   * `virtua` measures a row by its key, and Svelte redraws a block whose value
   * changed identity, so reusing the object is what keeps a repeated turn
   * from being drawn again.
   */
  private takePage(rows: unknown, cursor: string | null, direction: 'newest' | 'older'): void {
    this.inFlight = null;
    this.inner.update((held) => {
      const known = new Map(held.turns.map((turn) => [turn.key, turn]));
      const taken = new Set(known.keys());
      const named: Turn[] = [];
      for (const row of pageTurns(rows)) {
        // What the turn is held under: the fold's own name where it gave one,
        // and the name this conversation gave it where it did not. Reading
        // only the fold's name makes every unnamed turn a stranger on the way
        // back in, so the page draws it twice - once where it already was and
        // once where the page put it.
        const name = row.key ?? digest(row.messages);
        const repeated = known.get(name);
        if (repeated !== undefined) {
          named.push(repeated);
          continue;
        }
        const key = nameIn(row, taken);
        taken.add(key);
        const fresh: Turn = { key, messages: messagesOf(row), live: false };
        known.set(key, fresh);
        named.push(fresh);
      }
      const inPage = new Set(named.map((turn) => turn.key));
      // A page is the fold's own account of the turns it covers, so a turn the
      // frames built inside that range has been replaced by it and is dropped
      // rather than drawn twice - once as the frames had it, once as the fold
      // settled it.
      const rest = held.turns.filter((turn) => !turn.live && !inPage.has(turn.key));
      return {
        ...held,
        loaded: true,
        // The newest page's own handle names a place just above itself, which
        // is no use to a reader who has walked further back: taking it would
        // send the walk to the top of the conversation and fetch every page
        // between a second time. Only a page asked for BY cursor moves the
        // walk, and the first page establishes it.
        cursor: direction === 'older' || !held.loaded ? cursor : held.cursor,
        prepends: held.prepends + (direction === 'older' ? 1 : 0),
        turns: direction === 'older' ? [...named, ...rest] : [...rest, ...named],
      };
    });
  }

  /** One frame, folded into the turn it belongs to. */
  private takeUpdate(update: SessionUpdate): void {
    if (!sameSlot(slotOf(update), this.slot)) return;
    const variant = variantOf(update);
    if (variant === 'chat_appended') {
      const message = (update as { chat_appended?: { msg?: unknown } }).chat_appended?.msg;
      if (message === undefined) return;
      this.append(message);
      return;
    }
    // A turn that has settled is the server's fold's to draw, and the frames
    // that drew it were only ever a stand-in for it.
    if (variant === 'turn_complete' || variant === 'turn_cancelled' || variant === 'turn_error') {
      this.refresh();
    }
  }

  /**
   * One arriving message, into the turn it belongs to.
   *
   * It joins the live turn being written when there is one. With none open it
   * opens a turn of its own - unless the message is what a person said, which
   * always starts one, because that is the boundary the server pages on and a
   * prompt appended to the turn above it would draw the reader's own words
   * inside the answer to their last one.
   */
  private append(message: unknown): void {
    this.inner.update((held) => {
      const last = held.turns[held.turns.length - 1];
      if (last !== undefined && last.live && !opensATurn(message)) {
        const grown: Turn = { ...last, messages: [...last.messages, message] };
        return { ...held, turns: [...held.turns.slice(0, -1), grown] };
      }
      const taken = new Set(held.turns.map((turn) => turn.key));
      const key = nameIn({ key: liveName(message, held.turns.length), messages: [message] }, taken);
      // Every turn above it is the object it was: only the row that grew is
      // rebuilt, so growing one turn does not re-render the conversation.
      return { ...held, turns: [...held.turns, { key, messages: [message], live: true }] };
    });
  }
}

/** The turns a page carried, as the shape above, with anything else dropped. */
function pageTurns(rows: unknown): PageTurn[] {
  if (!Array.isArray(rows)) return [];
  const out: PageTurn[] = [];
  for (const row of rows) {
    if (row === null || typeof row !== 'object') continue;
    const { key, messages } = row as { key?: unknown; messages?: unknown };
    if (!Array.isArray(messages)) continue;
    out.push({ key: typeof key === 'string' ? key : null, messages });
  }
  return out;
}

/** Whether an update or a page belongs to one seat. */
function sameSlot(one: SessionSlot | null, other: SessionSlot): boolean {
  if (one === null) return false;
  return one.org === other.org && one.project === other.project && one.label === other.label;
}
