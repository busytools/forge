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

import { MORE_TURNS, slotOf, subjectKey } from '../protocol';
import type { ServerMessage, SessionUpdate } from '../protocol';
import { inFlightOf } from '../session/apply';
import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';
import { echoes } from './echoes.svelte';
import { fold, headingNameOf, namesSkill, queuedWords, skillBody } from './units';

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
   * The newest row of a conversation whose seat has a turn in flight.
   *
   * A second fact beside `live`, and it is kept current rather than
   * stamped: the core says whether a turn is running and a page read while one
   * is carries no `result` frame to say it, so a newer answer has to be able to
   * take this back. `live` cannot carry it, since a turn the frames built stays
   * live for good.
   */
  running?: boolean;
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

/** Whether a turn's own frames carry the frame it ends on - a result, or the CLI giving up. */
function carriesResult(messages: unknown[]): boolean {
  return messages.some((message) => {
    const type = (message as { type?: unknown } | null)?.type;
    return type === 'result' || type === 'error';
  });
}

/**
 * Whether a page row opens on a mid-turn prompt: the `queued_command` block
 * the scan hoists the transcript's `attachment` row into.
 *
 * The CLI queues a prompt only while a turn is running, so a row opening on
 * one is the turn above it carrying on rather than a turn of its own. The
 * harness's own background-completion notice rides the same block and opens no
 * turn at all, so it is not one of these.
 */
function queuedPrompt(row: PageTurn): boolean {
  return queuedWords(messagesOf(row)[0]) !== null;
}

/** Whether a seat's own record says a turn is in flight. */
function runningOf(data: unknown): boolean {
  const header = (data as { header?: { turn_in_flight?: unknown } } | null)?.header;
  return header?.turn_in_flight === true;
}

/**
 * Whether the core says a turn is running on `slot`, read from the connection's
 * own store.
 *
 * **This is the live answer, and the record a page draws is not.** The store
 * carries every frame the moment it lands, where a record is written once per
 * painted frame - so an event that has to know the state at this instant, like
 * a send deciding whether it started a turn, reads here, and a drawing reads
 * the record. What is held is the subscription's snapshot stepped by the frames
 * since, which is the same fold a seat answered from memory gets.
 *
 * `lastKnown` is the caller's own value, for a subject the connection has no
 * store for and for a tail the store dropped the head of.
 */
export function runningAt(
  connection: Pick<Connection, 'store'>,
  slot: SessionSlot,
  lastKnown: boolean,
): boolean {
  const store = connection.store({ session: slot });
  if (store === undefined) return lastKnown;
  const snapshot = store.snapshot();
  let held = snapshot === null ? lastKnown : runningOf(snapshot);
  // **A dropped tail is not replayed.** The store caps its updates and takes
  // the OLDEST off the front, and the `init` that opens a turn is the front of
  // it - so replaying an incomplete tail can miss the frame that opened the
  // turn and answer from a snapshot the frames since have left behind. The
  // snapshot's own answer is the honest one there.
  if (store.dropped() > 0) return held;
  for (const update of store.updates()) {
    const message = (update as { chat_appended?: { msg?: unknown } }).chat_appended?.msg;
    if (message !== undefined) held = inFlightOf(held, message);
  }
  return held;
}

/**
 * Whether a turn is still being written, which the fold cannot read off its
 * frames: a page read from a turn that has not ended carries no result frame,
 * because the transcript holds none either.
 *
 * Two carriers of the one fact. `live` is a turn the frames built, which stays
 * true until a page settles it; `running` is the core's own answer, kept
 * current so a turn that ends can take it back.
 *
 * **One copy, because two views fold from it**: the turn's row, and the strip
 * pinned above the box while the turn is written. A view that read this
 * differently would draw the running row of a turn the other has settled.
 */
export function beingWritten(turn: Turn): boolean {
  return turn.live || turn.running === true;
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
  /**
   * Whether the reader follows the newest end, which is the column's own
   * state and not something a comparison can derive: the terminal's
   * `auto_scroll`, with its transitions.
   */
  following: boolean;
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
  following: true,
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
  // **A delivered mid-turn prompt is carried by its attachment, and the id on
  // that row is the CLI's own.** The prompt's id - the one the forged row and
  // its lifecycle frames were sent under - rides the block instead, so a
  // reconciliation that read only the row's field would take the client's row
  // and the page's copy for two different messages, and draw one prompt twice.
  const frame = message as { uuid?: unknown; message?: { content?: unknown } } | null;
  const blocks = Array.isArray(frame?.message?.content) ? frame.message.content : [];
  for (const block of blocks) {
    const entry = block as { type?: unknown; source_uuid?: unknown } | null;
    if (entry?.type !== 'queued_command') continue;
    if (typeof entry.source_uuid === 'string' && entry.source_uuid !== '') {
      return entry.source_uuid;
    }
  }
  const id = frame?.uuid;
  return typeof id === 'string' && id !== '' ? id : null;
}

/**
 * The frame in `messages` that is the same message as `message` under its other
 * carrier - the words it says - or `null` when none is left to pair with it.
 *
 * **One frame answers for one copy**, which is why the frames that have already
 * answered are carried in: two prompts saying the same words inside one turn are
 * two messages, and the second would otherwise be read as a repeat of the first
 * and dropped - the reader's own words, gone.
 *
 * By identity rather than by position, because the frames it is asked about
 * belong to rows that are merged into one: a pairing counted by where a frame
 * sits is lost as soon as the row grows.
 */
function pairedWith(
  messages: unknown[],
  message: unknown,
  answered: ReadonlySet<unknown>,
): unknown {
  const words = wordsOf(message);
  if (words.length === 0) return null;
  for (const held of messages) {
    if (!answered.has(held) && sameWords(words, wordsOf(held))) return held;
  }
  return null;
}

/**
 * What one frame says, when it is a person's own words.
 *
 * The words are read from both carriers a prompt travels in: the frame's own
 * text blocks, and the `queued_command` block a page holds a mid-turn prompt
 * in.
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
  const queued = queuedWords(message);
  if (queued !== null) words.push(queued);
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
 * it says**, so an absent id read as "not carried" adds the same row a second
 * time. The copy is asked whether it says these words in any of its frames
 * rather than whether it says only them - the fold's own turns carry more than
 * one user row, and a comparison against the whole row finds neither of them.
 * A frame with neither an id nor words - a tool result, a thought - is never
 * found this way, which repeats it rather than dropping it.
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

/** Every content block of a frame, when it carries any. */
function blocksIn(message: unknown): unknown[] {
  const content = (message as { message?: { content?: unknown } } | null)?.message?.content;
  return Array.isArray(content) ? content : [];
}

/**
 * The skill a frame is the body of, or null for every other frame.
 *
 * The CLI injects a skill's body as a user frame; the fold pairs it with the
 * `Skill` call that loaded it, and this is the same reading for the store,
 * which is what keeps the frame in that call's turn.
 */
function skillNameOf(message: unknown): string | null {
  for (const block of blocksIn(message)) {
    const held = block as { type?: unknown; text?: unknown } | null;
    if (held?.type !== 'text' || typeof held.text !== 'string') continue;
    const skill = skillBody(held.text);
    if (skill !== null) return skill.name;
    // The carrier a tool-invoked skill uses: the skill's own markdown, named
    // by its title heading (`# PR Review Loop`), with no plumbing line.
    const titled = headingNameOf(held.text);
    if (titled !== null) return titled;
  }
  return null;
}

/**
 * Whether a frame is the harness's own line about an image it just read.
 *
 * It arrives as the reader's own row right behind the result that carried the
 * picture; the fold hangs it on the call that read the picture
 * (`imageNoteOf` in its `units.ts`), so the store keeps it in that call's turn
 * rather than letting it open one.
 */
function isImageNote(message: unknown): boolean {
  for (const block of blocksIn(message)) {
    const held = block as { type?: unknown; text?: unknown } | null;
    if (held?.type !== 'text' || typeof held.text !== 'string') continue;
    if (held.text.trim().startsWith('[Image: original ')) return true;
  }
  return false;
}

/** Whether a turn holds a result that carried an image. */
function holdsImageResult(turn: Turn): boolean {
  return turn.messages.some((held) =>
    blocksIn(held).some((block) => {
      const result = block as { type?: unknown; content?: unknown } | null;
      if (result?.type !== 'tool_result' || !Array.isArray(result.content)) return false;
      return result.content.some((inner) => (inner as { type?: unknown } | null)?.type === 'image');
    }),
  );
}

/** Whether a turn holds the `Skill` call a body of `name` belongs to. */
function holdsSkillCall(turn: Turn, name: string): boolean {
  return turn.messages.some((held) =>
    blocksIn(held).some((block) => {
      const use = block as { type?: unknown; name?: unknown; input?: unknown } | null;
      if (use?.type !== 'tool_use' || typeof use.name !== 'string') return false;
      if (use.name.toLowerCase() !== 'skill') return false;
      const want = (use.input as { skill?: unknown } | null)?.skill;
      return typeof want === 'string' && namesSkill(want, name);
    }),
  );
}

/** What a refused mode or model carries, as the wire names the fields. */
interface FailedLine {
  mode?: unknown;
  model?: unknown;
  message?: unknown;
}

/** Whether a frame is the core's own line, which stands alone where it must. */
function isForgeNotice(message: unknown): boolean {
  const frame = message as { type?: unknown; subtype?: unknown } | null;
  return frame?.type === 'system' && frame.subtype === 'forge_notice';
}

/** The update's variant name, for the ones the chat acts on. */
function variantOf(update: SessionUpdate): string | null {
  if (typeof update === 'string') return update;
  const [name] = Object.keys(update);
  return name ?? null;
}

/** One string field off an externally tagged update's payload. */
function textIn(update: SessionUpdate, variant: string, field: string): string | null {
  if (typeof update === 'string') return null;
  const payload = (update as Record<string, unknown>)[variant];
  if (payload === null || typeof payload !== 'object') return null;
  const value = (payload as Record<string, unknown>)[field];
  return typeof value === 'string' ? value : null;
}

/** How long a refused page waits before it is asked again, while the column is live. */
const RETRY_MS = 2_000;

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
  /**
   * The timer a refused page re-asks on, or `null`.
   *
   * A refusal is the server declining a page for a conversation it has not
   * attached yet, and it says so - asking again may find it. A refusal that
   * nothing asks again is that same dead end by another route: its words stand
   * over a conversation whose frames are landing, and everything said before
   * the client attached stays unreachable, because the page that would set the
   * walk's cursor never came.
   */
  private retry: ReturnType<typeof setTimeout> | null = null;
  /** This seat's subject key, which is how a snapshot is known to be its own. */
  private readonly key: string;
  /**
   * Whether the core says a turn is running on this seat, kept current from the
   * seat's own snapshots and the frames: `header.turn_in_flight` is the core's
   * answer, and a page read while one runs carries no `result` frame to say so.
   */
  private turnRunning = false;
  /**
   * The prompts the core has said are waiting in the CLI's queue, by the id
   * they were sent under, each with when this page saw it enter and the words
   * it carries.
   *
   * **The card in the pile is what draws a waiting prompt, and the chat holds
   * its forged row back.** Both copies of the words arrive at once - the core
   * announces the queue and forges the user turn beside it - so a view drawing
   * both would put "queued" in the pile under "sent" in the chat, about one
   * prompt. The pile is upstream's own answer to where the words wait, so the
   * chat holds its copy until the prompt starts.
   */
  private queued = new Map<string, { since: number; text: string }>();
  /**
   * Forged rows waiting for their prompt to start, by id - the core's own
   * user turn for words no view typed, held from arrival until the lifecycle
   * says the CLI took the prompt.
   */
  private drained = new Map<string, unknown>();

  constructor(connection: Connection, slot: SessionSlot) {
    this.connection = connection;
    this.slot = slot;
    this.key = subjectKey({ session: slot });
  }

  /**
   * The core's answer moved, so the newest row may have.
   *
   * **Re-answered rather than stamped.** A page read while a turn runs ends on
   * the turn being written and carries no `result` frame for it, so the row
   * cannot say it alone - and an answer only ever attached at ingest could
   * never be taken back, leaving a running bar on a turn that ended.
   */
  private heard(running: boolean): void {
    if (running === this.turnRunning) return;
    this.turnRunning = running;
    this.inner.update((held) => this.answered(held));
  }

  /**
   * The seat's own store, which is how a seat already visited answers.
   *
   * A return subscribes nothing - the subscription is the client's, kept for
   * the life of the connection - so no fresh answer is on its way, and what the
   * store holds is its last snapshot stepped by every frame since.
   */
  private heldRunning(): boolean {
    return runningAt(this.connection, this.slot, this.turnRunning);
  }

  /** The newest row carries the running row while the core says a turn is running. */
  private answered(held: Conversation): Conversation {
    const last = held.turns.length - 1;
    let changed = false;
    const turns = held.turns.map((turn, at) => {
      const wanted = at === last && this.turnRunning && !carriesResult(turn.messages);
      if ((turn.running === true) === wanted) return turn;
      changed = true;
      return { ...turn, running: wanted };
    });
    return changed ? { ...held, turns } : held;
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
    // Where the seat already is, for a seat a visit has answered before: a
    // return subscribes nothing, so nothing else would say.
    this.heard(this.heldRunning());
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
        this.clearRetry();
        this.ask(null);
      }
    });
    this.running = () => {
      stopMessages();
      stopStatus();
      this.clearRetry();
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

  /**
   * Where the reader is, which decides whether the newest turn is followed.
   *
   * **The rule is the terminal's, and it is exact.** `ChatViewport`'s
   * `auto_scroll` is re-engaged by its clamp only at `scroll_offset >=
   * max_scroll`, and disengaged by any scroll up - so a reader parked a few
   * pixels short of the end is reading, not following, and the column may not
   * move under them. A threshold here would pull them down mid-sentence.
   */
  following(follows: boolean): void {
    this.inner.update((held) =>
      held.following === follows ? held : { ...held, following: follows },
    );
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

  /**
   * Ask a refused page again, once a beat, while the column is live.
   *
   * **The refusal's own words are the instruction** ("asking again may find
   * it"), and the condition it refuses on - the core has not attached this
   * conversation yet - clears on its own once the seat's session is up. What
   * must not clear it is nothing: a reader who stays on the column never asks
   * again otherwise, and the history before the refusal stays unreachable.
   */
  private retryAsk(): void {
    if (this.retry !== null || this.running === null) return;
    this.retry = setTimeout(() => {
      this.retry = null;
      if (this.running === null) return;
      if (this.read().refused === null) return;
      this.ask(null);
    }, RETRY_MS);
  }

  /** A page landed, or the occupant changed: nothing is owed a re-ask. */
  private clearRetry(): void {
    if (this.retry !== null) {
      clearTimeout(this.retry);
      this.retry = null;
    }
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
        this.retryAsk();
        return;
      case 'snapshot':
        // The seat's own record, which is where the core's answer for a turn in
        // flight crosses: a page can be taken before this lands - `more` is
        // asked first - so it is read here rather than at the page.
        if (subjectKey(message.subject) === this.key) this.heard(runningOf(message.data));
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
    // A landed page is the refusal's ask answered: the timer that would ask
    // again is owed nothing, and the page's cursor is what the walk uses.
    this.clearRetry();
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
      /**
       * The frames that have answered for a copy of the reader's own words,
       * for the WHOLE page.
       *
       * The row it is read against spans rows: the fold opens a turn on every
       * queued prompt, so two of them arrive as two rows and both join the one
       * above - and a count minted per row forgets what the row before it
       * paired with, which lets the next copy consume the same frame and drop
       * the words.
       */
      const answered = new Set<unknown>();
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
        // **A row the fold cut at a mid-turn prompt is not a row of its own.**
        // The CLI queues a prompt only while a turn is running, and it writes
        // the transcript's row where it queued it - inside that turn - so the
        // row above it is the turn it interrupted, and the page's cut joins
        // back where the live path draws the words: inside the running row.
        const above = named[named.length - 1];
        if (above !== undefined && queuedPrompt(row)) {
          const messages = [...(copies[copies.length - 1] ?? [])];
          // The prompt the row already grew with is the SAME words under the
          // carrier a page holds them in, and the two carriers mint different
          // ids - so a copy is paired with the frame it is, and no other frame
          // answers for it twice.
          for (const message of messagesOf(row)) {
            if (carries(messages, message)) continue;
            const paired = pairedWith(messages, message, answered);
            if (paired === null) {
              // An unmatched copy lands here, at the END of the row, so where
              // its live echo never arrived it can sit below frames the row
              // already held (#1584).
              messages.push(message);
              answered.add(message);
            } else {
              answered.add(paired);
            }
          }
          named[named.length - 1] = { ...above, messages };
          copies[copies.length - 1] = messages;
          continue;
        }
        const key = nameIn(row, taken);
        taken.add(key);
        const fresh: Turn = { key, messages: messagesOf(row), live: false };
        known.set(key, fresh);
        named.push(fresh);
        copies.push(fresh.messages);
      }
      // **A page can carry the row the pile is already drawing.** The
      // transport keeps the forged turn it was sent, so a read taken while the
      // prompt still waits hands the words back as conversation - and a reader
      // attaching mid-queue would meet the card and the row at once. The
      // page's copies are quieted the same way the live frame is: a message
      // under a queued id is pulled back into the hold, and a row left with
      // nothing goes rather than drawing an empty one.
      const quiet: unknown[][] = [];
      const quietNamed: Turn[] = [];
      for (let at = 0; at < copies.length; at += 1) {
        const kept = (copies[at] ?? []).filter((message) => !this.waiting(message));
        if (kept.length === 0) continue;
        const row = named[at];
        if (row !== undefined) quietNamed.push({ ...row, messages: kept });
        quiet.push(kept);
      }
      if (quiet.length !== named.length) {
        named.splice(0, named.length, ...quietNamed);
        copies.splice(0, copies.length, ...quiet);
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
      // A row being written is not the page's to drop either way: `live` is a
      // turn the frames built, and `running` is the newest row of a seat the
      // core says has a turn in flight.
      const rest = held.turns.filter(
        (turn) => !(turn.live || turn.running === true) && !inPage.has(turn.key),
      );
      // A turn being written that no row of this page accounts for is kept - a
      // page of OLDER turns, or one serialized before those frames landed -
      // because dropping it leaves the reader's own words nowhere, with nothing
      // asking for them again.
      //
      // **A row the page DID account for goes with it, by key**, which is what
      // the page's own names cover: a row the core says is running but the
      // frames did not build would otherwise be drawn beside its own repeat,
      // under one key - which a keyed list throws on.
      const loose = held.turns.filter(
        (turn) => (turn.live || turn.running === true) && !inPage.has(turn.key),
      );
      return this.answered({
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
      });
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
    // The occupant that left took its answer with it, and nothing about the new
    // one is known until its own record or frames say - the queue it held
    // included.
    this.queued.clear();
    this.drained.clear();
    this.turnRunning = false;
    this.inner.set(NOTHING);
    this.clearRetry();
    this.ask(null);
  }

  /** One frame, folded into the turn it belongs to. */
  private takeUpdate(update: SessionUpdate): void {
    if (!sameSlot(slotOf(update), this.slot)) return;
    const variant = variantOf(update);
    if (variant === 'prompt_queued') {
      const uuid = textIn(update, 'prompt_queued', 'uuid');
      const words = textIn(update, 'prompt_queued', 'text');
      if (uuid !== null) {
        this.queued.set(uuid, { since: Date.now(), text: words ?? '' });
        // **The two frames race, and this side of the race is the retraction.**
        // The dispatcher emits the user turn as it routes the prompt; the
        // task's queue announcement follows it by a flush, so a frame often
        // arrives before the pile knows the prompt is waiting. An announcement
        // for a row the seat has already drawn pulls it back into the hold -
        // in the same flush that drew it, so nothing flickers.
        this.retract(uuid);
      }
      return;
    }
    // The seat's rows are the pile's, and nothing about a lifecycle frame is
    // conversation - but its id is what lets a held row go.
    if (variant === 'prompt_lifecycle') {
      this.advanced(update);
      return;
    }
    if (variant === 'chat_appended') {
      const message = (update as { chat_appended?: { msg?: unknown } }).chat_appended?.msg;
      if (message === undefined) return;
      // The forged row for a prompt the pile is drawing waits with it: the
      // card holds the words while the CLI queues them, and this row draws
      // them the moment it starts. Only a user turn counts: a dispatch is the
      // one thing that forges a prompt id onto a frame.
      const id = uuidOf(message);
      if (id !== null && (message as { type?: unknown }).type === 'user' && this.queued.has(id)) {
        this.drained.set(id, message);
        return;
      }
      // Stepped BEFORE the row is written, so a row that opens or grows answers
      // from the new state rather than the one before the frame.
      this.turnRunning = inFlightOf(this.turnRunning, message);
      this.append(message);
      return;
    }
    // The seat changed occupant under this column - a `/new`, a `/resume`, a
    // login or a logout - so what is drawn is another session's conversation.
    if (variant === 'session_replaced') {
      this.replaced();
      return;
    }
    // A process that is gone takes its queue with it: no lifecycle frame is
    // coming for anything it held, so the waits go rather than standing
    // forever. A fresh connect is the same fact from the other side.
    if (variant === 'connection_failed' || variant === 'connected') {
      this.queued.clear();
      this.drained.clear();
      return;
    }
    // A turn that has settled is the server's fold's to draw, and the frames
    // that drew it were only ever a stand-in for it.
    if (variant === 'turn_complete' || variant === 'turn_cancelled' || variant === 'turn_error') {
      this.heard(false);
      this.refresh();
    }
    // The core's own line: a command's answer, or why one did not run. It is
    // drawn here because this store is the conversation the page draws, and the
    // record's copy of the transcript is not. **Live only, deliberately**: the
    // CLI never wrote such a row, so a page that attaches afterwards has
    // nothing to read it from and the line is not owed to it.
    if (variant === 'notice') {
      const line = (update as { notice?: { severity?: unknown; text?: unknown } }).notice;
      const text = line?.text;
      if (typeof text !== 'string' || text === '') return;
      this.append({ type: 'system', subtype: 'forge_notice', severity: line?.severity, text });
      return;
    }
    // A mode or a model the CLI refused. It answers through no frame of its
    // own either, so it is the same line: what was asked for, and the CLI's own
    // words for the refusal.
    if (variant === 'set_mode_failed' || variant === 'set_model_failed') {
      const payload = (update as { set_mode_failed?: FailedLine; set_model_failed?: FailedLine })[
        variant
      ];
      if (payload === undefined) return;
      const asked = typeof payload.mode === 'string' ? payload.mode : payload.model;
      const why = typeof payload.message === 'string' ? payload.message : '';
      const what = typeof asked === 'string' && asked !== '' ? asked : 'the session';
      this.append({
        type: 'system',
        subtype: 'forge_notice',
        severity: 'error',
        text: `${what} was refused: ${why}`.trimEnd(),
      });
    }
  }

  /**
   * Whether this message is the row of a prompt the pile is still holding,
   * taking it into the hold on the way.
   *
   * The first frame kept is the one that draws at the drain: a live frame the
   * seat already had is preferred over a page's later copy of it.
   */
  private waiting(message: unknown): boolean {
    const id = uuidOf(message);
    if (id === null || !this.queued.has(id)) return false;
    if (!this.drained.has(id)) this.drained.set(id, message);
    return true;
  }

  /**
   * Pull back a row the seat drew for a prompt the pile has just claimed, so
   * one prompt is not drawn by the chat and the pile at once.
   *
   * A turn the retraction empties goes with it rather than standing as a row
   * with nothing in it - the drain opens a fresh turn for the row when the
   * prompt starts, which is the same turn the reader ends up seeing.
   */
  private retract(uuid: string): void {
    let pulled: unknown;
    this.inner.update((held) => {
      let changed = false;
      const turns: Turn[] = [];
      for (const turn of held.turns) {
        const hit = turn.messages.find((message) => uuidOf(message) === uuid);
        if (hit === undefined) {
          turns.push(turn);
          continue;
        }
        changed = true;
        pulled = hit;
        const messages = turn.messages.filter((message) => uuidOf(message) !== uuid);
        if (messages.length > 0) turns.push({ ...turn, messages });
      }
      return changed ? { ...held, turns } : held;
    });
    if (pulled !== undefined) this.drained.set(uuid, pulled);
  }

  /**
   * The CLI moved a prompt, from the frame that names only its id and state.
   *
   * **`started` is the drain**: the held row draws, carrying how long the
   * prompt waited - the wait being the whole difference between the card's
   * "queued" and the row's "sent", and the reason the row was held at all.
   * `refused` and `discarded` draw too, without the note, because those words
   * never reached a model and the pile's own ending line is the only other
   * place they exist. `cancelled` goes: the reader deleted it, and the send
   * that was waiting on it is settled rather than left behind a copy that is
   * never coming.
   */
  private advanced(update: SessionUpdate): void {
    const uuid = textIn(update, 'prompt_lifecycle', 'uuid');
    const state = textIn(update, 'prompt_lifecycle', 'state');
    if (uuid === null || state === null) return;
    const entry = this.queued.get(uuid);
    const held = this.drained.get(uuid);
    if (entry === undefined && held === undefined) return;
    const dropped = state === 'cancelled' || state === 'refused' || state === 'discarded';
    const drawn = state === 'started' || state === 'completed';
    if (!dropped && !drawn) return;
    this.queued.delete(uuid);
    this.drained.delete(uuid);
    if (state === 'cancelled') {
      // Only when the pending send is this prompt's own: the store holds one
      // send per seat, so a second send's mark must not go with a first
      // prompt's cancel.
      if (entry !== undefined && echoes.of(this.key)?.words === entry.text) {
        echoes.clear(this.key);
      }
      return;
    }
    if (held === undefined) return;
    const waited = entry === undefined ? 0 : Date.now() - entry.since;
    this.append(dropped ? held : this.noted(held, waited));
  }

  /**
   * The row with its wait written on it, in the pile's own vocabulary.
   *
   * A wait under a second is the idle send, where the frames land inside one
   * beat and naming a wait would invent one: the note is "sent" alone.
   */
  private noted(message: unknown, waitedMs: number): unknown {
    if (typeof message !== 'object' || message === null) return message;
    const seconds = Math.floor(waitedMs / 1000);
    const note =
      seconds < 1
        ? 'sent'
        : `queued ${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')} · sent`;
    return { ...message, forge_note: note };
  }

  /**
   * One arriving message, into the turn it belongs to.
   *
   * Where it goes is the `opens` below, read there rather than restated here:
   * the row above takes it, one opens for it, or it is held nowhere.
   *
   * **What a person said joins a turn being written like anything else.** The
   * CLI fuses a mid-turn prompt into the turn it interrupted rather than
   * opening one, and the terminal keeps its clock: a mid-turn submit there
   * carries the live bar onto a fresh tail placeholder instead of restarting
   * it. One turn stays one row here, with the words drawn inside it and the
   * clock the row opened with; a row of its own leaves one turn as two rows,
   * both counting - which is the whole of what the join decides.
   *
   * **A frame the fold draws nothing out of never opens a row: it joins the
   * turn it arrived in, settled or not, and is held nowhere when there is
   * none.** Such a row holds a row's space while drawing nothing, and a row
   * the reader never scrolls to keeps the list's estimate rather than its own
   * height - a blank row per tool call on a running seat, and thousands a
   * minute from the CLI's thinking-token counter.
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
      // estimate rather than its own height. A tool result is one of these, and
      // it arrives on a running seat between one turn and the next.
      const units = fold([message]);
      const draws = units.length > 0;
      // **A prompt brings the reader back to the end.** It is the one frame
      // that is the reader's OWN words, and the terminal engages its follow on
      // the prompt path for the same reason: they have just asked for
      // something to arrive, so a column left where they had scrolled to would
      // hide the very answer they are waiting on.
      const follow = held.following || units.some((unit) => unit.kind === 'user');
      // **A skill's body belongs to the turn whose `Skill` call loaded it.**
      // The CLI injects the frame mid-turn, but it can arrive above a settled
      // turn, and a row of its own is a second telling of the same thing under
      // the reader's name. The turn is found by the skill's own name, the same
      // match the fold pairs the two by.
      const skill = skillNameOf(message);
      if (skill !== null) {
        for (let at = held.turns.length - 1; at >= 0; at -= 1) {
          const target = held.turns[at];
          if (target === undefined || !holdsSkillCall(target, skill)) continue;
          const grown: Turn = { ...target, messages: [...target.messages, message] };
          return this.answered({
            ...held,
            turns: [...held.turns.slice(0, at), grown, ...held.turns.slice(at + 1)],
          });
        }
      }
      // **The harness's image line joins the turn that read the picture**, for
      // the same reason: it is that call's row caption, and a turn of its own
      // draws it as a separating row of the reader's.
      if (isImageNote(message)) {
        for (let at = held.turns.length - 1; at >= 0; at -= 1) {
          const target = held.turns[at];
          if (target === undefined || !holdsImageResult(target)) continue;
          const grown: Turn = { ...target, messages: [...target.messages, message] };
          return this.answered({
            ...held,
            turns: [...held.turns.slice(0, at), grown, ...held.turns.slice(at + 1)],
          });
        }
      }
      // A frame that draws opens a row of its own only where no turn is being
      // written. A row the core says is running is one of those too, whichever
      // way the client learned it: a seat reached mid-turn has its row from a
      // page, and a frame that opened a second row beside it would be one turn
      // in two.
      //
      // **Being written is asked of the fold's own rule rather than of `live`**
      // - a turn the frames built stays live for good (#1486), so a row that
      // carries the frame it ended on is over whatever the flags say, and a
      // prompt arriving after it must still open the next turn's row.
      const writing = last !== undefined && beingWritten(last) && !carriesResult(last.messages);
      const opens =
        draws &&
        !isSystem(message) &&
        (last === undefined || (!writing && (opensATurn(message) || !beingWritten(last))));
      // **The core's own line is the one `system` frame that opens a row**, and
      // only where there is no turn to join. Its only copy is this frame - the
      // CLI wrote none - so on a seat with no turn yet, which is every fresh
      // one, holding it back drops it rather than placing it, and the reader's
      // own words draw with no answer under them.
      if (!opens && !(last === undefined && draws && isForgeNotice(message))) {
        if (last === undefined) return held;
        const grown: Turn = { ...last, messages: [...last.messages, message] };
        return this.answered({
          ...held,
          following: follow,
          turns: [...held.turns.slice(0, -1), grown],
        });
      }
      const taken = new Set(held.turns.map((turn) => turn.key));
      const key = nameIn({ key: liveName(message, held.turns.length), messages: [message] }, taken);
      // **A row opened for the core's own line is not a turn being written.**
      // The line is a command's answer, so there is no turn in flight and the
      // core's own header says so; a live row would draw the running strip and
      // its clock for it, which nothing would clear but a later page. It is
      // also what the page's next account replaces a live row with - so a row
      // marked live here would be consumed by a turn settling that has nothing
      // to do with it.
      const live = !isForgeNotice(message);
      // Every turn above it is the object it was: only the row that grew is
      // rebuilt, so growing one turn does not re-render the conversation.
      return this.answered({
        ...held,
        following: follow,
        turns: [...held.turns, { key, messages: [message], live }],
      });
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
