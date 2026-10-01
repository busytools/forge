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
import { fold } from './units';

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
  /**
   * The names a page knows this turn by, when it settled one the frames built.
   *
   * A live turn keeps the name its row already had - a list keys its rows by
   * that name, so handing it the page's own closes every disclosure the reader
   * had open - and the page's name is kept here so the next page, which repeats
   * the turn under it, finds this row rather than drawing a second.
   */
  also?: string[];
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
 * **The turn's FIRST message id**, because a turn can grow: a frame that draws
 * nothing joins the turn it arrived in, and a name digested from the whole
 * message list then disagrees with the page that repeats the grown turn - the
 * row already held is left where it is and the turn draws twice.
 *
 * The digest is the fallback for a message with no id, and it is not a hash
 * for safety: the worst a collision does is hand two turns one name, which is
 * what the ordinal in `nameIn` is for.
 */
function nameOf(messages: unknown[]): string {
  const id = (messages[0] as { uuid?: unknown } | undefined)?.uuid;
  if (typeof id === 'string' && id !== '') return `turn-${id}`;
  return digest(messages);
}

/**
 * A digest of a turn's whole content, for a turn whose first message has no id.
 *
 * **Not a substitute for `nameOf`**: it covers every message, so a turn that
 * GROWS takes a different name from one that does not - the rename `nameOf`
 * exists to prevent. It is reached only by a page whose turn opens on a
 * message with no id, which the wire does not send, and nothing covers it.
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
  const base = turn.key ?? nameOf(turn.messages);
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

/** A frame's own id, or `null` when it carries none. */
function uuidOf(message: unknown): string | null {
  const id = (message as { uuid?: unknown } | null)?.uuid;
  return typeof id === 'string' && id !== '' ? id : null;
}

/**
 * What one frame says, when it is a person's own words.
 *
 * A delivery row is a display-only user turn with no id at all, and its words
 * are the only thing its two copies agree on.
 */
function wordsOf(message: unknown): string[] {
  const words: string[] = [];
  if ((message as { type?: unknown } | null)?.type !== 'user') return words;
  const content = (message as { message?: { content?: unknown } | null } | null)?.message?.content;
  if (!Array.isArray(content)) return words;
  for (const block of content) {
    const text = (block as { text?: unknown } | null)?.text;
    if (typeof text === 'string' && text !== '') words.push(text);
  }
  return words;
}

/** Whether two frames say the same words, in the same order. */
function sameWords(one: string[], other: string[]): boolean {
  return one.length === other.length && one.every((word, at) => word === other[at]);
}

/**
 * Whether a copy already carries a frame.
 *
 * The frame's own id where it has one. **A frame with no id is found by what
 * it says**: a delivery row is forged rather than read off the wire and
 * carries no id on purpose, so an absent id read as "not carried" adds the
 * same row a second time. The copy is asked whether it says these words in any
 * of its frames rather than whether it says only them - the fold's own turns
 * carry more than one user row, and a comparison against the whole row finds
 * neither of them. A frame with neither an id nor words - a tool result, a
 * thought - is never found this way, which repeats it rather than dropping it.
 *
 * `prose` narrows the arm at one of the four call sites, which says why there:
 * an id is unique and needs no guard, while the words are what a delivery row
 * shares with the page's copy of it - and what a turn an id-bearing frame
 * opened must NOT be matched by.
 */
function carries(messages: unknown[], message: unknown, prose = true): boolean {
  const id = uuidOf(message);
  if (id !== null) return messages.some((held) => uuidOf(held) === id);
  if (!prose) return false;
  const words = wordsOf(message);
  if (words.length === 0) return false;
  return messages.some((held) => sameWords(words, wordsOf(held)));
}

/**
 * Whether a frame opens a turn of its own rather than joining the live one:
 * what a person said.
 *
 * Its answer decides only while a turn is live: a non-`system` frame that
 * draws something and arrives above a settled turn opens one whatever this
 * says. **And a frame the fold draws nothing out of is not a row at all**,
 * whichever branch it takes - a call's own result arrives in a user frame and
 * draws nothing on its own, which is what holds a call and the frames that
 * update it in one turn.
 */
function opensATurn(message: unknown): boolean {
  const type = (message as { type?: unknown } | null)?.type;
  return type === 'user';
}

/** Whether a frame is a `system` frame, which is a report about a turn rather than part of one. */
function isSystem(message: unknown): boolean {
  return (message as { type?: unknown } | null)?.type === 'system';
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
  /**
   * Answers still coming for asks this conversation stopped wanting.
   *
   * A seat that changes occupant clears what is held and asks again, and the
   * ask made before the swap is answered anyway - a page carries neither an id
   * nor an occupant, so this is the whole of what can tell them apart. A
   * `session_id` on the page is the real fix and is a wire change.
   */
  private abandoned = 0;
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
   * the newest turns rather than the whole conversation; the page draws a
   * window of those and asks `more` for what is above, so the ask is where
   * its turns come from.
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
      // A dropped socket takes any page in flight with it, and the reconnect
      // answers with an ask of its own - so an ask this conversation was told
      // to forget is answered by nothing, and its count must not outlive it.
      if (status !== 'open') {
        this.inFlight = null;
        this.abandoned = 0;
      } else {
        this.ask(null);
      }
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
    return this.ask(cursor);
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

  private ask(before: string | null): boolean {
    if (this.inFlight !== null) return false;
    this.inFlight = before === null ? 'newest' : 'older';
    // A closed socket answers nothing, so the flag must not stay set waiting
    // on a page that was never asked for - and the caller has to know, because
    // what it does with the answer is hold a reader's place while it arrives.
    if (this.connection.more(this.slot, before, MORE_TURNS)) return true;
    this.inFlight = null;
    return false;
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
        // A refused ask is answered by no page at all, so the ask it belongs to
        // is over - and a count of asks this conversation was told to forget is
        // spent on pages that are never coming.
        this.inFlight = null;
        this.abandoned = 0;
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
    if (this.abandoned > 0) {
      // The answer to an ask the conversation stopped wanting: a seat that
      // changed occupant under it cleared what was held and asked again, and a
      // page carries neither an id nor an occupant - so a count of abandoned
      // asks is the only thing that can tell this answer from the new one.
      this.abandoned -= 1;
      return;
    }
    this.inFlight = null;
    this.inner.update((held) => {
      const known = new Map(held.turns.map((turn) => [turn.key, turn]));
      // A turn a page has settled is also known by the page's own name for it,
      // which is what the next page repeats it under.
      for (const turn of held.turns) {
        for (const alias of turn.also ?? []) known.set(alias, turn);
      }
      const taken = new Set(known.keys());
      const named: Turn[] = [];
      /** Each row's messages after the reconciliation, which hold the page's own. */
      const copies: unknown[][] = [];
      for (const row of pageTurns(rows)) {
        // What the turn is held under: the fold's own name where it gave one,
        // and the name this conversation gave it where it did not. Reading
        // only the fold's name makes every unnamed turn a stranger on the way
        // back in, so the page draws it twice - once where it already was and
        // once where the page put it.
        const name = row.key ?? nameOf(row.messages);
        const repeated = known.get(name);
        if (repeated !== undefined) {
          // A repeated row is handed back as the object already held, and the
          // page's copy of it can be the NEWER of the two - a read taken after
          // frames the held row was built before. So the two are reconciled by
          // what each is missing, and the held object is kept only where the
          // page says nothing new: a row the reader is looking at is not drawn
          // again by an answer that says nothing.
          const missed = repeated.messages.filter((message) => !carries(row.messages, message));
          const adds = row.messages.filter((message) => !carries(repeated.messages, message));
          const messages =
            adds.length === 0
              ? repeated.messages
              : missed.length === 0
                ? row.messages
                : [...row.messages, ...missed];
          const settled = messages === repeated.messages ? repeated : { ...repeated, messages };
          named.push(settled);
          copies.push(settled.messages);
          continue;
        }
        const key = nameIn(row, taken);
        taken.add(key);
        const fresh: Turn = { key, messages: messagesOf(row), live: false };
        known.set(key, fresh);
        named.push(fresh);
        copies.push(fresh.messages);
      }
      // A row the page settled: a turn the server has an END for, which is a
      // `result` frame where the wire carries one and any row but the last
      // otherwise - the last row is the transcript's own tail, and that is the
      // one turn a page can have been read while it was still being written.
      const settledRow = (index: number): boolean =>
        copies[index]?.some(
          (message) => (message as { type?: unknown } | null)?.type === 'result',
        ) === true || index !== named.length - 1;
      // A live turn and a page row are the same exchange when they share a
      // frame: frames belong to one turn, so a shared id is that turn.
      //
      // **A row with no id at all reconciles on its prose**, which is the only
      // thing its two copies agree on - a delivery row is forged rather than
      // read off the wire, and an id invented for it would read as a match
      // where there is none. That path is taken only for the row a page can
      // have been read while it was still being written: the LAST row, with no
      // result frame. A settled row is an exchange that is over, so it cannot
      // be the turn still arriving - and matching it replaces the live row
      // with an older one that merely says the same words, which drops the row
      // the reader just received.
      const shares = (messages: unknown[], turn: Turn, unsettled: boolean): boolean => {
        // **The words arm is for the turn a frame with no id OPENED**, and for
        // nothing else. A turn an id-bearing frame opened is reconciled by ids,
        // and a words-only match hands it a row that is not its own: a repeat
        // takes the exchange's row and gives it the repeat's opening frame,
        // which in the shorter shape leaves the exchange with no row at all.
        const words = unsettled && uuidOf(turn.messages[0]) === null;
        return turn.messages.some((message) => carries(messages, message, words));
      };
      const replaced = new Set<Turn>();
      const drawn = named.map((row, index) => {
        // The exchange is the row's messages AFTER the reconciliation above,
        // which hold the page's own copy of it: a repeated row is handed back
        // as the held object, whose messages can be the older of the two, so a
        // row taken before that reconciliation reads as not carrying what the
        // page plainly carries.
        const copy = copies[index] ?? [];
        // One row per live turn: a turn this page already drew is not the
        // exchange a later row of the same page is an account of. It is this
        // that keeps two rows from landing under one key, which the virtualised
        // list throws on.
        const live = held.turns.find(
          (turn) => turn.live && !replaced.has(turn) && shares(copy, turn, !settledRow(index)),
        );
        if (live === undefined) return row;
        // The page is the account of the turn it copies, so the live turn is
        // replaced either way - and where the copy is settled it is the whole
        // account, so the row stands as it is.
        replaced.add(live);
        // **The row keeps the name it already had.** A page that settles a live
        // turn is an account of the SAME exchange, and a list keys its rows by
        // that name - so handing it the page's own unmounts the row and draws a
        // fresh one, which closes every disclosure the reader had open and
        // reopens every one they had closed.
        const name = live.key;
        // The page's own name for it is kept beside: the next page repeats the
        // turn under that name, and a turn that cannot be found by the name the
        // page uses is drawn again beside the row already held.
        const also =
          live.also?.includes(row.key) === true ? live.also : [...(live.also ?? []), row.key];
        if (settledRow(index)) return { ...row, key: name, also };
        // A copy read while the turn was still being written is not: the row
        // stands in its place, marked live, and carries both the words no frame
        // did (the CLI never echoes a prompt) and the frames the page was read
        // too early to have - so the frames still to come join it rather than
        // opening a second row.
        const grown = live.messages.filter((message) => !carries(copy, message));
        return { key: name, messages: [...copy, ...grown], live: true, also };
      });
      // The page's own names count as being on the page: a turn it settled is
      // held under the name its row already had, and the copy the page carried
      // is the same turn rather than another row to keep beside it.
      const inPage = new Set(drawn.flatMap((turn) => [turn.key, ...(turn.also ?? [])]));
      const rest = held.turns.filter((turn) => !turn.live && !inPage.has(turn.key));
      // A live turn no row of this page shares a frame with is one the page
      // cannot be an account of - a page of OLDER turns, or one serialized
      // before those frames landed - and it is kept: dropping it there leaves
      // the reader's own words nowhere, with nothing asking for them again.
      const loose = held.turns.filter((turn) => turn.live && !replaced.has(turn));
      return {
        ...held,
        loaded: true,
        // A page that lands is the ask the refusal spoke for, answered - the
        // refusal is about THAT ask rather than about the conversation, and
        // one that outlived its ask leaves every later page undrawable.
        refused: null,
        // The newest page's own handle names a place just above itself, which
        // is no use to a reader who has walked further back: taking it would
        // send the walk to the top of the conversation and fetch every page
        // between a second time. Only a page asked for BY cursor moves the
        // walk, and the first page establishes it.
        cursor: direction === 'older' || !held.loaded ? cursor : held.cursor,
        prepends: held.prepends + (direction === 'older' ? 1 : 0),
        turns:
          direction === 'older' ? [...drawn, ...rest, ...loose] : [...rest, ...drawn, ...loose],
      };
    });
  }

  /**
   * Start over for the occupant that just arrived.
   *
   * The turns go rather than being replaced in place, because a page that
   * answered before the swap is the OLD occupant's and nothing in a page says
   * which occupant it came from - so what is held has to be nothing, and the
   * ask has to go out after the swap rather than before it.
   */
  private replaced(): void {
    if (this.inFlight !== null) this.abandoned += 1;
    this.inFlight = null;
    this.inner.set(NOTHING);
    this.ask(null);
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
    // The seat changed occupant under this column - a `/new`, a `/resume`, a
    // login or a logout - so what is drawn is another session's conversation.
    if (variant === 'session_replaced') {
      this.replaced();
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
   *
   * **A frame the fold draws nothing out of never opens a row: it joins the
   * turn it arrived in, settled or not, and is held nowhere when there is
   * none.** Such a row holds a row's space while drawing nothing, and a row
   * the reader never scrolls to keeps the list's estimate rather than its own
   * height - a blank row per tool call on a running seat, and thousands a
   * minute from the CLI's thinking-token counter. That is the server's
   * boundary too: a turn opens for a person's own words and for nothing else -
   * a prompt the CLI queued mid-turn included, though that one reaches a page
   * through the transcript rather than through here - and everything else
   * draws inside the turn already open.
   *
   * Holding a frame that belongs to no turn loses nothing informative: a page
   * carries a turn's messages from its first, so the opening rows ride the
   * first page. That page is `start`'s ask or a reconnect - the update that
   * would ask for one when a turn settles is defined and never sent - so the
   * window is a round trip rather than a turn.
   */
  private append(message: unknown): void {
    this.inner.update((held) => {
      const last = held.turns[held.turns.length - 1];
      // What a row of its own would hold, asked of the fold rather than of the
      // frame's shape: a frame it draws nothing out of is never a row, whatever
      // its type or subtype, because such a row holds a row's space and draws
      // nothing - and a row the reader never scrolls to keeps the list's
      // estimate rather than its own height. A tool result and an assistant
      // frame carrying only thinking are two of these, and both arrive on a
      // running seat between one turn and the next.
      const draws = fold([message]).length > 0;
      // A turn opens where a person's own words do, while a turn is live: the
      // server's own rule, so everything else joins the turn it arrived in,
      // settled or not. A frame arriving with no turn at all is held nowhere,
      // which loses nothing - a page carries a turn's messages from its first.
      const opens =
        draws && !isSystem(message) && (last === undefined || opensATurn(message) || !last.live);
      if (!opens) {
        if (last === undefined) return held;
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
