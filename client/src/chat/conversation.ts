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

import { writable, type Readable } from 'svelte/store';

import { MORE_TURNS, slotOf, subjectKey } from '../protocol';
import type { ServerMessage, SessionUpdate } from '../protocol';
import { whenPainted } from '../paint';
import { inFlightOf } from '../session/apply';
import { watchSession } from '../session/live';
import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';
import { echoes } from './echoes.svelte';
import { outputs } from './outputs.svelte';
import { fold, headingNameOf, namesSkill, queuedWords, skillBody } from './units';
import { onRefusal } from '../refusals';
import { callOutputOf } from '../wire/call-output';

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
  /**
   * A turn whose content is in the pile's card, kept in the list as an empty
   * row at the height it measured.
   *
   * **The row is never removed for the hold.** The list's virtualiser caches
   * row sizes positionally and splices a removed last item's away, so the
   * send's own pull-and-return drew the re-added row at the estimate for one
   * frame - the chat-wide up-and-down on every Enter (#1890, confirmed live
   * 2026-10-09). Kept keyed and present, the size survives; the wrapper's
   * remembered height keeps the row's space while it draws nothing, and the
   * drain fills it in place.
   */
  held?: boolean;
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
 * Whether a connection failure reads as the accounts being rate limited.
 *
 * The terminal's own substring rule, matched arm for arm: the core surfaces
 * no typed variant for it, and a false positive costs a recoverable explainer
 * instead of the raw error.
 */
function rateLimitedFailure(message: string): boolean {
  const held = message.toLowerCase();
  return (
    (held.includes('rate') && held.includes('limit')) ||
    held.includes('rate-limited') ||
    held.includes('rate_limited') ||
    held.includes('all accounts')
  );
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
  /**
   * How many asks this conversation was told to forget - a dropped socket or a
   * refusal - and no page will ever answer.
   *
   * **The column keeps a count of the asks it is holding, and this is what
   * drains it.** A page landing drains one; a forgotten ask drains nothing,
   * because the page that would have is never coming - so without this the
   * column's own count outlives the ask, and everything gated on it (the
   * prepend compensation, and the anchor's restore standing out of its way)
   * stays on for the life of the seat.
   */
  dropped: number;
}

/** A conversation nothing has answered yet. */
export const NOTHING: Conversation = {
  turns: [],
  loaded: false,
  cursor: null,
  refused: null,
  following: true,
  prepends: 0,
  dropped: 0,
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
 * How long a publish waits for a paint before it goes out anyway.
 *
 * `requestAnimationFrame` is the coalescer, not the guarantee: a scheduled
 * callback that never fires - whatever lost it, which is nothing this code
 * can know - would leave the column dead, new rows folded and never drawn,
 * until a read landed, because the flag clears only inside that callback.
 * A painting page clears the deadline long before it: a frame is 16ms at
 * 60Hz and this is a quarter second.
 */
const PAINT_WATCHDOG_MS = 250;

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
  /** How many readers draw this conversation, which is when a redraw is waited for. */
  private readers = 0;
  /**
   * The conversation as folded, which every frame moves at once.
   *
   * **The fold is immediate and only the draw waits.** A frame is applied the
   * moment it arrives - the fold is cheap, and the record it leaves is what
   * the next fold reads - while `inner`, what a page draws, is written once
   * per painted frame. So the burst a return delivers - every frame that
   * arrived while the page was away - is one draw of the latest rather than a
   * replay of the whole queue, and nothing is dropped: the held record is
   * exact throughout. That is the same split the seat's own record runs, and
   * for the same reason (`session/live.ts`).
   */
  private held: Conversation = NOTHING;
  /** The frame a publish is waiting for, or `null`. */
  private queued: number | null = null;
  /** The paint's own deadline, armed while `queued` is: see [`PAINT_WATCHDOG_MS`]. */
  private watchdog: ReturnType<typeof setTimeout> | null = null;
  private readonly inner = writable<Conversation>(NOTHING, () => {
    this.readers += 1;
    return () => {
      this.readers -= 1;
      // The last reader has gone, so nothing left is owed a paint: the record
      // waiting for one is written now, which is what a return draws.
      if (this.readers === 0) this.flush();
    };
  });
  /**
   * The page being waited on, which is both the guard against a second ask
   * queueing behind it and the direction the answer goes: a page asked for by
   * cursor belongs ABOVE what is held, and one asked for without belongs at
   * the end of it.
   */
  private inFlight: 'newest' | 'older' | null = null;

  /**
   * Whether the seat's page was last announced as on screen.
   *
   * Read only by `showing()`'s own idempotence guard; the DRAW gate is the
   * column's (a left seat's value lands in `kept`, not on the screen). The
   * fold never consulted it (Ved, 2026-10-09).
   */
  private shown = true;

  /**
   * A return whose ask was swallowed by one already in flight.
   *
   * The stale page is not the read a return needs, so the want is held and
   * fired when that ask settles - the shape `session/live.ts`'s
   * `replaceWanted` has.
   */
  private returnWanted = false;
  /**
   * Answers still coming for asks this conversation stopped wanting.
   *
   * A seat that changes occupant clears what is held and asks again, and the
   * ask made before the swap is answered anyway - a page carries neither an id
   * nor an occupant, so this is the whole of what can tell them apart. A
   * `session_id` on the page is the real fix and is a wire change.
   */
  private abandoned = 0;
  /**
   * Frames that arrived with no turn to join and no row of their own.
   *
   * **Nothing the seat sent is dropped for want of a turn** (rule 25): the
   * turn a counter or a task frame belongs to may simply not have opened yet,
   * so they wait here and ride the next turn that opens - or join the newest
   * row a landing page brings, where the page was cut before they arrived.
   * A page that already carries one takes its place and the copy here is let
   * go, which is the frame's own copy arriving a second time rather than a
   * second frame.
   */
  private unturned: unknown[] = [];
  /**
   * The frames a ride PLACED in a row, so a page can heal them.
   *
   * A ridden frame sits in a row the page's own cut may not own - a notice
   * that opened on an empty seat, a live turn - and only a live row is ever
   * reconciled by the merge. So each placed frame is kept here, and a page
   * that carries its own copy of one lets the placed copy go (from whichever
   * row holds it) before the merge runs, which is what keeps one frame from
   * being drawn twice through a later page. It also keeps a ridden frame from
   * standing as the evidence `shares()` matches a live row to an older turn:
   * the frame belongs where the page put it, not where the ride parked it.
   *
   * Unhealed entries are live-only frames no page will ever carry, so the set
   * holds at most the frames that rode before the first page landed - no
   * bound of its own, and nothing clears it on `connected`, where `waiting`
   * and `drained` do: a reconnect re-reads the seat rather than replacing its
   * conversation, so the first page after it heals what it can and the rest
   * draw where they were placed.
   */
  private ridden = new Set<unknown>();
  /** Record that an ask which was in flight is now answered by nothing. */
  private forgot(): void {
    this.fold((held) => ({ ...held, dropped: held.dropped + 1 }));
  }
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
  private waiting = new Map<string, { since: number; text: string }>();
  /**
   * Forged rows waiting for their prompt to start, by id - the core's own
   * user turn for words no view typed, held from arrival until the lifecycle
   * says the CLI took the prompt.
   */
  private drained = new Map<string, unknown>();
  /**
   * Whether a drop was seen, and whether a snapshot ever reconciled.
   *
   * **The two states that make a snapshot authoritative**: the first one
   * after a subscribe (the attacher's only word) and the first one after a
   * socket drop (the frames of anything that settled in the gap are gone).
   * Every later snapshot is only a picture, and a picture the server built
   * before a send can be delivered after it - see {@link armed}.
   */
  private missed = false;
  private reconciled = false;
  /**
   * The newest held turn a newest page failed to reach, while the
   * conversation walks down to it.
   *
   * **A read that can be short walks on its own** (Ved, 2026-10-09): the
   * held window's floor can sit inside the very stretch a client missed -
   * a suspended page's gap - and the stretch between the page and the held
   * tail is in no single page. The pages between come from the transcript
   * rather than wait for a scroll. Set when a newest page shares nothing
   * with the held turns; the walk stops when a page reaches this turn, the
   * history ends, or a page repeats the cursor it was asked with.
   */
  private walkTo: string | null = null;
  /** The cursor the in-flight ask went out with, so a page that repeats it stops the walk. */
  private askedBefore: string | null = null;
  /**
   * The held tail's turn keys when the walk started: the marks a walk page
   * inserts ABOVE.
   *
   * A walk page is the middle of the conversation - newer than the tail it
   * walks toward, older than the page it walks from - so the plain older
   * landing (which prepends above everything held) would strand it out of
   * order. The keys split the held turns into "below this page" (the tail)
   * and "above it" (the fills so far and the newest page), and the walk's
   * page lands between them.
   */
  private walkBelow: Set<string> | null = null;

  constructor(connection: Connection, slot: SessionSlot) {
    this.connection = connection;
    this.slot = slot;
    this.key = subjectKey({ session: slot });
  }

  /** Write the fold as it stands, dropping the paint it was waiting for. */
  private flush(): void {
    if (this.queued !== null) {
      cancelAnimationFrame(this.queued);
      this.queued = null;
    }
    if (this.watchdog !== null) {
      clearTimeout(this.watchdog);
      this.watchdog = null;
    }
    this.inner.set(this.held);
  }

  /**
   * Publish the fold on the next painted frame, or at once while nobody draws
   * it. The record has already moved by the time this is called: this is only
   * the draw.
   */
  private soon(): void {
    if (this.queued !== null) return;
    if (this.readers === 0) {
      this.flush();
      return;
    }
    // A hidden page has no frame to wait for, so `whenPainted` flushes at
    // once and answers `null`: nothing to cancel, and no deadline owed.
    this.queued = whenPainted(() => this.flush());
    if (this.queued === null) return;
    this.watchdog = setTimeout(() => this.flush(), PAINT_WATCHDOG_MS);
  }

  /**
   * Apply `fn` to the fold and draw it now.
   *
   * For what is not a frame in a stream - a page's answer, a refusal, the
   * reader's own follow decision, the swap: each is a state rather than one
   * step of one, so it is written at once, the way the seat's own record
   * writes a read's answer (`session/live.ts`).
   */
  private fold(fn: (held: Conversation) => Conversation): void {
    this.held = fn(this.held);
    this.flush();
  }

  /** Apply one STREAM frame, whose draw lands on the next painted frame. */
  private stream(fn: (held: Conversation) => Conversation): void {
    this.held = fn(this.held);
    this.soon();
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
    this.fold((held) => this.answered(held));
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
    return {
      subscribe: (fn) => {
        // **Every reader arrives on the fold as it stands**, not on the last
        // painted state: a frame's draw may still be waiting for its paint,
        // and whoever attaches now must be handed the newest. This flushes
        // for the second reader as much as the first, which the writable's
        // own start hook cannot do.
        this.flush();
        return this.inner.subscribe(fn);
      },
    };
  }

  /**
   * Hold the seat, listen to it, and ask for its newest page.
   *
   * **The hold lives with the conversation, not with the page** (Ved,
   * 2026-10-09): the server sends a seat's frames to a subscriber of that
   * seat, so a subscription given back on leave would make "every frame folds
   * for every seat" untrue for exactly the frames that matter. A kept
   * conversation keeps receiving; the DRAW is what waits.
   *
   * **It is a HOLD on the seat's one subscription, not a subscribe of its
   * own.** The page opens that subscription and holds it too, and two
   * subscribes are two whole records encoded and sent to a client that keeps
   * one copy - 6.5 MB twice on a large seat, measured 2026-10-09. The
   * subscription goes with the last holder, so a page leaving a seat this
   * conversation still folds sends nothing at all.
   *
   * **And a subscription is not what fills the list.** The snapshot carries
   * the newest turns rather than the whole conversation; the page draws a
   * window of those and asks `more` for what is above, so the ask is where
   * its turns come from.
   */
  start(): () => void {
    if (this.running !== null) return this.running;
    const release = watchSession(this.connection, this.slot, false).hold();
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
        // Anything the queue settles while the socket is down dies unheard,
        // so the next snapshot is the only word on those holds - `armed`
        // reads this rather than trusting every snapshot's listing.
        this.missed = true;
        const held = this.inFlight !== null;
        this.inFlight = null;
        this.abandoned = 0;
        // A held return-want dies with the socket: the reconnect answers with
        // an ask of its own, and firing the want after it would be a second
        // ask for the same page.
        this.returnWanted = false;
        // And the column is TOLD, not left counting: its own twin of this ask
        // is what holds the prepend compensation on, and nothing else drains
        // it. Only an ask actually in flight counts - a closed socket that was
        // already idle has nothing to forget.
        if (held) this.forgot();
      } else {
        this.clearRetry();
        this.ask(null);
      }
    });
    // A dispatch refused before it left the browser draws its line here: the
    // command's own seat is the column it was sent from, so a line for another
    // seat belongs to that seat's conversation, not this one.
    const stopRefusals = onRefusal((line) => {
      if (line.seat !== subjectKey({ session: this.slot })) return;
      // `appendOnce`: the same line twice in a row is one row, so two Enters
      // on a dead socket draw the refusal once.
      this.appendOnce({
        type: 'system',
        subtype: 'forge_notice',
        severity: 'warning',
        text: line.text,
      });
    });
    this.running = () => {
      stopMessages();
      stopStatus();
      stopRefusals();
      release();
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
  refresh(): boolean {
    return this.ask(null);
  }

  /**
   * The seat's page has come on screen, which resumes the DRAW. The state
   * never waited: every frame folded wherever the seat was, so a return
   * draws what is already held.
   *
   * The refresh stays as a reconcile, for a read that failed or a stamp
   * missed while the seat was away. **An ask already in flight swallows
   * it**, so the want is held and fired when the ask settles.
   */
  showing(): void {
    if (this.shown) return;
    this.shown = true;
    if (!this.refresh()) this.returnWanted = true;
  }

  /** A return whose refresh was swallowed: fire it now that the ask settled. */
  private returnIfWanted(): void {
    if (!this.returnWanted) return;
    this.returnWanted = false;
    this.ask(null);
  }

  /** The seat's page has gone: the draw stops, the fold does not. */
  leaving(): void {
    this.shown = false;
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
    this.fold((held) => (held.following === follows ? held : { ...held, following: follows }));
  }

  private read(): Conversation {
    return this.held;
  }

  private ask(before: string | null): boolean {
    if (this.inFlight !== null) return false;
    this.inFlight = before === null ? 'newest' : 'older';
    this.askedBefore = before;
    // A closed socket answers nothing, so the flag must not stay set waiting
    // on a page that was never asked for - and the caller has to know, because
    // what it does with the answer is hold a reader's place while it arrives.
    if (this.connection.more(this.slot, before, MORE_TURNS)) return true;
    this.inFlight = null;
    this.askedBefore = null;
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
        // **And a refusal for ANOTHER seat is not this conversation's.** The
        // connection is shared, so every chat hears every error: a background
        // seat's refusal - the everyday no-session-yet state, re-asking every
        // couple of seconds - would otherwise drain THIS seat's count of asks
        // and let the restore run in the middle of a prepend it must leave
        // alone. A seatless refusal, from a server that predates the field, is
        // read the old way: it is this seat's.
        if (message.seat !== undefined && subjectKey({ session: message.seat }) !== this.key)
          return;
        // A refused ask is answered by no page at all, so the ask it belongs to
        // is over - and a count of asks this conversation was told to forget is
        // spent on pages that are never coming.
        this.inFlight = null;
        this.abandoned = 0;
        // A refusal is a settled ask like any other, and what a return wanted
        // is what the retry below asks for - the want goes with it rather
        // than riding a second ask at the same refused core.
        this.returnWanted = false;
        this.fold((held) => ({
          ...held,
          refused: message.why,
          loaded: true,
          dropped: held.dropped + 1,
        }));
        this.retryAsk();
        return;
      case 'snapshot':
        // The seat's own record, which is where the core's answer for a turn in
        // flight crosses: a page can be taken before this lands - `more` is
        // asked first - so it is read here rather than at the page. **The
        // queue rides it too**, and that is what arms the hold for the reader
        // the hold was built for: a client attaching mid-queue gets no backlog
        // of `prompt_queued` frames - the pending backlog goes to the
        // first-ever subscriber alone - so without this read a fresh load, a
        // refresh or a seat switch mid-queue meets the card and the row at
        // once, the exact duplicate the quieting exists to stop.
        if (subjectKey(message.subject) === this.key) {
          this.heard(runningOf(message.data));
          this.armed(message.data);
        }
        return;
      case 'update':
        // **Every frame folds, whatever the seat is doing** (Ved,
        // 2026-10-09): the state is lossless for every seat, and whether a
        // seat is shown gates only what is DRAWN - the column hands a left
        // seat's value to `kept` rather than to the screen. Dropping frames
        // for an unshown seat made its state the place a conversation was
        // lost: the frames that crossed while a page was suspended or a
        // seat was away were gone, and the return's reads could not restore
        // what the bounded window had itself already dropped.
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
  /**
   * The record with every placed ride the page's own cut accounts for taken
   * back out, and those frames released from the ridden set.
   *
   * A ride parks a frame in whatever row was there to take it, and the page's
   * own copy of the same frame can sit in a row the merge never reconciles -
   * a notice that opened on an empty seat is not live, and only live rows are
   * matched. Removing the parked copy BEFORE the merge runs is what keeps one
   * frame from drawing twice, and it also keeps the parked copy from tying a
   * live row to a page row that ran before it.
   *
   * Answers the record it was handed when the page carries none of them, so a
   * page that heals nothing leaves every row the object it was.
   */
  private healedOf(held: Conversation, fromPage: unknown[]): Conversation {
    if (this.ridden.size === 0) return held;
    const carried = new Set([...this.ridden].filter((message) => carries(fromPage, message)));
    if (carried.size === 0) return held;
    const turns = held.turns.map((turn) => {
      const kept = turn.messages.filter((message) => !carried.has(message));
      return kept.length === turn.messages.length ? turn : { ...turn, messages: kept };
    });
    for (const message of carried) this.ridden.delete(message);
    return { ...held, turns };
  }

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
    const askedBefore = this.askedBefore;
    this.askedBefore = null;
    this.clearRetry();
    const pageRows = pageTurns(rows);
    const fromPage = pageRows.flatMap((row) => messagesOf(row));
    /**
     * What this landing found, read after the fold for the walk's own step.
     *
     * A holder rather than two `let`s: the fold writes them from its closure,
     * and TypeScript does not widen a captured `let` after the call - the
     * reads below would be narrowed to `never`.
     */
    const walk: {
      key: string | null;
      below: Set<string> | null;
      reached: Set<string> | null;
      fill: boolean;
    } = {
      key: null,
      below: null,
      reached: null,
      fill: false,
    };
    this.fold((held) => {
      const first = !held.loaded;
      const healed = this.healedOf(held, fromPage);
      const known = new Map(healed.turns.map((turn) => [turn.key, turn]));
      // A turn a page has settled is also known by the page's own name for it,
      // which is what the next page repeats it under.
      for (const turn of healed.turns) {
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
      for (const row of pageRows) {
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
      // under a queued id is pulled back into the hold. **The splice fires on
      // ANY pull, not only on a row that emptied**: the ordinary shape is a
      // turn opening at the forged row with the running frames joined to it,
      // so the row survives the pull - and a copy kept while its words went
      // into the hold would draw beside the card AND drain a second time.
      const quiet: unknown[][] = [];
      const quietNamed: Turn[] = [];
      let pulled = false;
      for (let at = 0; at < copies.length; at += 1) {
        const copy = copies[at] ?? [];
        const kept = copy.filter((message) => !this.heldBack(message));
        if (kept.length !== copy.length) pulled = true;
        if (kept.length === 0) continue;
        const row = named[at];
        if (row !== undefined) quietNamed.push({ ...row, messages: kept });
        quiet.push(kept);
      }
      if (pulled) {
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
        // A frame the ride PLACED is already gone from these rows when the
        // page carries its own copy: `healedOf` took it back out before this
        // match runs, which is what keeps the parked copy from tying a live
        // row to a page row that ran before it.
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
        const live = healed.turns.find(
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
      // Frames that waited for a turn, against this page: one the page already
      // carries is the SAME frame read back, so its copy here is let go; the
      // rest are newer than the page's cut, and they ride its newest row -
      // which is the exchange that was being written when they arrived. An
      // older page takes none of them: they are newer than everything in it.
      if (direction === 'newest' && this.unturned.length > 0) {
        const riding = this.unturned.filter((message) => !carries(fromPage, message));
        const newest = drawn[drawn.length - 1];
        if (newest === undefined) {
          // Nothing on this page to ride; the next turn that opens takes them.
          this.unturned = riding;
        } else {
          if (riding.length > 0) {
            for (const frame of riding) this.ridden.add(frame);
            drawn[drawn.length - 1] = { ...newest, messages: [...newest.messages, ...riding] };
          }
          this.unturned = [];
        }
      }
      // The page's own names count as being on the page: a turn it settled is
      // held under the name its row already had, and the copy the page carried
      // is the same turn rather than another row to keep beside it.
      const inPage = new Set(drawn.flatMap((turn) => [turn.key, ...(turn.also ?? [])]));
      // **A newest page that shares no frame with what was held is a
      // different reach of the conversation**, matched by the merge's own
      // frame carries - a repeated row, a live row's copy, a row the fold cut
      // at a mid-turn prompt. The turns in between are in no page, and
      // `older()` walks from the OLDEST held - so keeping the stale tail
      // above the new page stranded them mid-column for good (rule 25's
      // rows). The page's own cursor is the walk-back, so the held turns go
      // with this merge and a reader scrolling up loads the history again
      // from the transcript.
      const reachesHeld =
        direction === 'older' ||
        healed.turns.some((turn) =>
          turn.messages.some((message) =>
            pageRows.some((row) => carries(messagesOf(row), message)),
          ),
        );
      // A row being written is not the page's to drop either way: `live` is a
      // turn the frames built, and `running` is the newest row of a seat the
      // core says has a turn in flight.
      const rest = healed.turns.filter(
        (turn) => !(turn.live || turn.running === true) && !inPage.has(turn.key),
      );
      // **A newest page that does not reach the held state starts the walk.**
      // The held tail is never dropped for it (Ved, 2026-10-09): the compare
      // names the newest held turn no page has covered yet, the pages between
      // come from the transcript - `older()` walks from the page's own cursor
      // - and the tail keeps drawing throughout rather than waiting on a
      // scroll. The stop is a page reaching that turn, the history's end, or
      // a cursor that repeats.
      //
      // **Only a turn a page can carry is a target.** A `forge_notice` line
      // is written here and nowhere else - no read restores it - so a tail of
      // nothing but notices has nothing to walk for, and targeting one would
      // fetch the whole history one page per landing for a turn no page will
      // ever carry. The walk's own reach is the transcript-carried turns.
      if (direction === 'newest' && !reachesHeld) {
        const carriable = rest.filter((turn) =>
          turn.messages.some((message) => !isForgeNotice(message)),
        );
        walk.key = carriable.length > 0 ? (carriable[carriable.length - 1]?.key ?? null) : null;
        walk.below = walk.key === null ? null : new Set(rest.map((turn) => turn.key));
      }
      // The fresh fill: a FIRST landing that brought nothing but one running
      // turn has history above it that no reader can reach without a scroll.
      walk.fill = direction === 'newest' && first && drawn.length <= 1 && cursor !== null;
      walk.reached = inPage;
      // A turn being written that no row of this page accounts for is kept - a
      // page of OLDER turns, or one serialized before those frames landed -
      // because dropping it leaves the reader's own words nowhere, with nothing
      // asking for them again.
      //
      // **A row the page DID account for goes with it, by key**, which is what
      // the page's own names cover: a row the core says is running but the
      // frames did not build would otherwise be drawn beside its own repeat,
      // under one key - which a keyed list throws on.
      const loose = healed.turns.filter(
        (turn) => (turn.live || turn.running === true) && !inPage.has(turn.key),
      );
      return this.answered({
        ...healed,
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
        cursor: direction === 'older' || !held.loaded || !reachesHeld ? cursor : held.cursor,
        prepends: held.prepends + (direction === 'older' ? 1 : 0),
        turns:
          direction === 'older'
            ? this.walkTo !== null && this.walkBelow !== null
              ? [
                  // **A walk page lands in the middle, not at the front.**
                  // The plain older landing prepends above everything held,
                  // which is true for a scroll (its page is older than all of
                  // it) and false for a walk (its page is newer than the held
                  // tail it walks toward). The tail's keys split the held
                  // turns, and the page lands between them - so the recovered
                  // stretch draws where it happened, not above the history.
                  ...rest.filter((turn) => this.walkBelow?.has(turn.key) === true),
                  ...drawn,
                  ...rest.filter((turn) => this.walkBelow?.has(turn.key) !== true),
                  ...loose,
                ]
              : [...drawn, ...rest, ...loose]
            : [...rest, ...drawn, ...loose],
      });
    });
    // The walk's own step, after the fold: the held tail is never dropped,
    // and when this landing did not cover the newest held turn the next page
    // down is asked now - one page per landing, so the socket is never
    // flooded, and the stop is the turn reached, the history's end, or a
    // cursor that did not move.
    if (walk.key !== null) {
      this.walkTo = walk.key;
      this.walkBelow = walk.below;
    }
    if (this.walkTo !== null) {
      const target = this.walkTo;
      if (walk.reached !== null && walk.reached.has(target)) {
        this.walkTo = null;
        this.walkBelow = null;
      } else if (cursor === null || cursor === askedBefore) {
        this.walkTo = null;
        this.walkBelow = null;
      } else {
        this.older();
      }
    }
    // **One pull on a fresh open, not a chain.** A first landing that brought
    // nothing but one running turn asks once for the history above it - enough
    // to show there is something up - and the reader's own walking fills as
    // they move (Ved, 2026-10-09: "it should pull at least something so that I
    // know that there is something up. And as I move up, it should start
    // filling up"). The ask guard makes a second call a no-op when the walk
    // above already asked.
    if (walk.fill) {
      this.older();
    }
    this.returnIfWanted();
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
    const held = this.inFlight !== null;
    if (held) this.abandoned += 1;
    this.inFlight = null;
    // The occupant that left took its answer with it, and nothing about the new
    // one is known until its own record or frames say - the queue it held
    // included.
    this.waiting.clear();
    this.drained.clear();
    this.turnRunning = false;
    // Frames the last occupant's run left waiting for a turn go with it: they
    // are its conversation, and the new one's rows are not where they belong.
    // The rides it left behind go too - their rows are gone with the swap, and
    // the record is only ever read against rows this conversation holds. The
    // read answers go the same way: they are about calls the new occupant
    // never made.
    this.unturned = [];
    this.ridden.clear();
    // A walk was walking toward a turn of the conversation that is gone.
    this.walkTo = null;
    this.walkBelow = null;
    this.askedBefore = null;
    // A return's want is about a conversation that is gone.
    this.returnWanted = false;
    outputs.clear(this.key);
    // A swap is not a frame's draw: the reset lands now, whatever any paint
    // was waiting for.
    this.held = NOTHING;
    this.flush();
    // **The swap forgot an ask too, and it has to say so.** The reset above
    // zeroes the count the column drains against, so a column that was holding
    // an ask keeps holding it - the drain never fires, `shift` stays armed for
    // the life of the seat, and the observer's restore never runs for a parked
    // reader (measured: the offset left at 50 where the row above them had
    // moved it to 290).
    if (held) this.forgot();
    this.clearRetry();
    this.ask(null);
  }

  /** One frame, folded into the turn it belongs to. */
  private takeUpdate(update: SessionUpdate): void {
    // An update naming a seat that is not this one is another conversation's.
    // **A KEYLESS update is everyone's** - the service report and the fatal
    // are app-level and arrive on the connection's home read, which the shell
    // always holds - so it passes this door and the arms below decide whether
    // it draws here.
    const at = slotOf(update);
    if (at !== null && !sameSlot(at, this.slot)) return;
    const variant = variantOf(update);
    if (variant === 'prompt_queued') {
      const uuid = textIn(update, 'prompt_queued', 'uuid');
      const words = textIn(update, 'prompt_queued', 'text');
      if (uuid !== null) {
        this.waiting.set(uuid, { since: Date.now(), text: words ?? '' });
        // **The card is what carries a waiting prompt's words, so the send's
        // own mark goes with them.** The mark stands in for the row the words
        // will occupy, and while the prompt waits the pile's card IS that row -
        // left up, the mark draws the words a second time, in the chat, saying
        // "sending" over a card already showing them (Ved, 2026-10-04). Ids
        // only: two sends of the same text compare equal by words.
        if (echoes.of(this.key)?.id === uuid) echoes.clear(this.key);
        // **The two frames race, and this side of the race is the retraction.**
        // The dispatcher emits the user turn as it routes the prompt; the
        // task's queue announcement follows it, so a frame often arrives before
        // the pile knows the prompt is waiting. An announcement for a row the
        // seat has already drawn pulls it back into the hold - and what keeps
        // that invisible is the SERVER's batch window rather than this side:
        // the transport coalesces both updates into one flush, so the draw and
        // the retraction land in one paint. Outside that window the row hops
        // back into the pile a beat late rather than standing in both.
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
      if (id !== null && (message as { type?: unknown }).type === 'user' && this.waiting.has(id)) {
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
    if (variant === 'connected') {
      // A process that is gone takes its queue with it - and a row the hold
      // already emptied is drawn bare rather than left standing empty: cleared
      // bookkeeping alone strands it, no drain fills it, and the page that
      // later carries the prompt draws it a second time beside the blank row.
      this.releaseHeld();
      this.waiting.clear();
      this.drained.clear();
      return;
    }
    // The answer to the row's own ask, kept where the row reads it: by the
    // call's id, in the store the row holds, because it is content of a row
    // and not a frame of any turn - no page will carry it back.
    if (variant === 'call_output') {
      const answered = (update as { call_output?: { call_id?: unknown; output?: unknown } })
        .call_output;
      const callId = answered?.call_id;
      if (typeof callId === 'string') {
        outputs.post(this.key, callId, callOutputOf(answered?.output));
      }
      return;
    }
    // The failure itself is drawn, not only the roster row's reason: the
    // terminal answers one with a chat line - the rate-limit explainer when
    // the accounts are exhausted, the raw why otherwise (#1638). The
    // terminal's own input-lock tail is deliberately not ported: "Press
    // Ctrl+Q" is that view's input model, and the page's box is its own.
    if (variant === 'connection_failed') {
      this.waiting.clear();
      this.drained.clear();
      const failed = (update as { connection_failed?: { message?: unknown } }).connection_failed;
      const why = failed?.message;
      if (typeof why === 'string' && why !== '') {
        this.append(
          rateLimitedFailure(why)
            ? {
                type: 'system',
                subtype: 'forge_notice',
                severity: 'warning',
                text: 'Waiting for account reset; click another project or wait.',
              }
            : {
                type: 'system',
                subtype: 'forge_notice',
                severity: 'error',
                text: `Connection failed: ${why}`,
              },
        );
      }
      return;
    }
    // The core's fatal, announced before the process goes (#1638): keyless,
    // so every open conversation draws it, and the words are the server's own
    // - what the terminal prints on exit. A repeat is one row.
    if (variant === 'fatal_error') {
      const fatal = (update as { fatal_error?: { message?: unknown } }).fatal_error;
      const said = fatal?.message;
      if (typeof said === 'string' && said !== '') {
        this.appendOnce({
          type: 'system',
          subtype: 'forge_notice',
          severity: 'error',
          text: `forge stopped: ${said}`,
        });
      }
      return;
    }
    // The service status the core watches for the whole install (#1638). It is
    // keyless on the wire, so every open conversation draws it as the terminal
    // pushes it - one line per report, and a repeat is one row.
    if (variant === 'service_status') {
      const report = (update as { service_status?: { severity?: unknown; message?: unknown } })
        .service_status;
      const said = report?.message;
      if (typeof said === 'string' && said !== '') {
        this.appendOnce({
          type: 'system',
          subtype: 'forge_notice',
          severity: report?.severity === 'error' ? 'error' : 'warning',
          text: said,
        });
      }
      return;
    }
    // A turn that has settled is the server's fold's to draw, and the frames
    // that drew it were only ever a stand-in for it.
    if (variant === 'turn_complete' || variant === 'turn_cancelled' || variant === 'turn_error') {
      // **The record is the truth for a COMPLETION, not this frame.** The
      // CLI ends a turn per delivered prompt, so a mid-turn message turns
      // one outward turn into several CLI turns - and each completion fires
      // here while the seat is still running. `heard(false)` on its own
      // killed the live row (its clock, its thinking count) until a
      // remount; the store's record says whether the seat has a turn in
      // flight. A genuine end still settles at once: the refresh below
      // draws the page that carries its result. **An error or a cancel is
      // an end in itself** - the core says so on this frame - so it takes
      // the bar down directly.
      if (variant === 'turn_complete') {
        this.heard(this.heldRunning());
      } else {
        this.heard(false);
      }
      this.refresh();
      // The plan-limit next steps ride the turn's own failure (#1638), where
      // the core's class is known: the terminal's words and its numbered
      // steps, reflowed onto the one line this page's notices draw, with the
      // core's own message where the terminal's summary rides - it is not
      // drawn a line above when the refusal never reached the CLI. Class-only
      // deliberately: the terminal's fallback classifier is its own, and a
      // hint the core did not classify stays off. The auth and input-lock
      // hints stay unported: the auth line names a terminal command where the
      // page has a sign-in state, and Ctrl+Q is that view's input model.
      const failed = (update as { turn_error?: { class?: unknown; message?: unknown } }).turn_error;
      if (variant === 'turn_error' && failed?.class === 'plan_limit') {
        const why =
          typeof failed.message === 'string' && failed.message !== '' ? `: ${failed.message}` : '';
        this.appendOnce({
          type: 'system',
          subtype: 'forge_notice',
          severity: 'error',
          text: `Turn blocked by account or plan limits${why}. Next steps: 1. Wait a few minutes and retry. 2. Reduce request size or request frequency. 3. Check quota/billing for your account or switch plans.`,
        });
      }
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
    // A worker's review turn ended and the core batched the tally onto this
    // session's line, which the terminal draws as an info line on the
    // reviewer's own chat; the same line here, for the same reason - no
    // transcript row holds it (#1776). The tally itself (which the terminal
    // also parks for its badge) has no client surface to draw on.
    if (variant === 'review_activity_notice') {
      const notice = (update as { review_activity_notice?: { message?: unknown } })
        .review_activity_notice;
      const text = notice?.message;
      if (typeof text !== 'string' || text === '') return;
      this.append({ type: 'system', subtype: 'forge_notice', severity: 'info', text });
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
   * Reconcile the hold against the read's own queue, which is the reader the
   * hold was built for and the invariant that keeps it from stranding.
   *
   * **The hold must never outlive the queue's listing of the id.** A socket
   * drop loses the lifecycle frames of anything that settles during the gap,
   * and the reconnect's snapshot is the first word after it - so a uuid the
   * snapshot no longer lists has settled, and its held row releases: the
   * words draw rather than being filtered from every surface forever with
   * the card gone too. In the other direction a listed uuid ARMS - the read
   * that hands an attacher its card is the only word that reader gets - and
   * any copy a page drew before this snapshot arrived is pulled back into
   * the hold, the same way the live retraction pulls one.
   */
  private armed(data: unknown): void {
    // **Only the snapshots that are the ONLY word on a uuid reconcile it: the
    // first after a start (a fresh attacher is handed no `prompt_queued`
    // backlog) and the first after a drop (the lifecycle frames of anything
    // that settled in the gap died with the socket).** A later healthy
    // snapshot can be older than the live frames it meets - a read the server
    // built before a send can be delivered after it - and acting on its
    // listing would undo what the frames just settled: releasing a hold draws
    // a row the pile is still drawing, and arming one retracts a row that
    // already started.
    const fresh = !this.reconciled || this.missed;
    this.reconciled = true;
    // `state` is the wire's own record inside the snapshot, the same nesting
    // `sessionFrom` reads: the queue is a sibling of `scan_cwd` there.
    const root = (data ?? {}) as { state?: unknown };
    const state = (root.state ?? {}) as { queue?: unknown };
    const queue = state.queue;
    if (!Array.isArray(queue)) return;
    const listed = new Set<string>();
    for (const row of queue) {
      const entry = row as { uuid?: unknown; text?: unknown } | null;
      if (typeof entry?.uuid !== 'string' || entry.uuid === '') continue;
      listed.add(entry.uuid);
      if (!fresh) continue;
      if (this.waiting.has(entry.uuid) || this.drained.has(entry.uuid)) continue;
      this.waiting.set(entry.uuid, {
        since: Date.now(),
        text: typeof entry.text === 'string' ? entry.text : '',
      });
      // A page asked before this snapshot landed draws the forged row
      // unfiltered; arming now pulls that copy back into the hold.
      this.retract(entry.uuid);
    }
    if (!fresh) return;
    for (const uuid of [...this.waiting.keys()]) {
      if (!listed.has(uuid)) this.release(uuid);
    }
    for (const uuid of [...this.drained.keys()]) {
      if (!listed.has(uuid)) this.release(uuid);
    }
    this.missed = false;
  }

  /**
   * A held prompt the read no longer lists: it settled while this page was
   * not listening, so its row draws - bare, because nothing here observed
   * whether a turn took it - and a later page pairs with it by id.
   *
   * **Only a pulled copy draws.** A uuid that only ever ARMED - the snapshot
   * listed it, and no page or frame has carried it yet - releases to nothing,
   * which is right for a cancelled prompt and self-healing for one that
   * completed: the page's own copy reaches the conversation on the next read.
   */
  private release(uuid: string): void {
    const held = this.drained.get(uuid);
    this.waiting.delete(uuid);
    this.drained.delete(uuid);
    if (held !== undefined) this.append(held);
  }

  /**
   * Draw every held row bare, its queue forgotten.
   *
   * A hold whose bookkeeping is being cleared - a reconnect, a failed
   * connection - would otherwise leave an emptied row that no drain fills and
   * no page reconciles. The words come back the way a settled ask's do.
   */
  private releaseHeld(): void {
    for (const uuid of [...this.drained.keys()]) this.release(uuid);
    for (const uuid of [...this.waiting.keys()]) this.release(uuid);
  }

  /**
   * Whether this message is the row of a prompt the pile is still holding,
   * taking it into the hold on the way.
   *
   * Whichever pull names a uuid last holds the frame that draws at the
   * drain - the pulls do not order themselves, and every copy says the same
   * words, so the choice only decides which one stands.
   */
  private heldBack(message: unknown): boolean {
    const id = uuidOf(message);
    if (id === null || !this.waiting.has(id)) return false;
    this.drained.set(id, message);
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
    this.stream((held) => {
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
        // **The emptied row stays, marked held.** Dropped, the list splices its
        // size away and the drain's re-add flashes (#1890); kept, the row holds
        // its measured space and the words land back in it.
        turns.push(messages.length > 0 ? { ...turn, messages } : { ...turn, messages, held: true });
      }
      return changed ? { ...held, turns } : held;
    });
    if (pulled !== undefined) this.drained.set(uuid, pulled);
  }

  /**
   * The CLI moved a prompt, from the frame that names only its id and state.
   *
   * **The per-state table.** `started` is the drain: the held row draws,
   * carrying how long the prompt waited - the wait being the whole difference
   * between the card's "queued" and the row's "sent", and the reason the row
   * was held at all. `completed` draws the same way, which is what saves a
   * hold whose `started` was lost - a reattached client that missed the frame.
   * `refused` and `discarded` draw too, without the note, because those words
   * never reached a model and the pile's own ending line is the only other
   * place they exist. `cancelled` goes: the reader deleted it, and the send
   * that was waiting on it is settled rather than left behind a copy that is
   * never coming. Anything else keeps the hold.
   */
  private advanced(update: SessionUpdate): void {
    const uuid = textIn(update, 'prompt_lifecycle', 'uuid');
    const state = textIn(update, 'prompt_lifecycle', 'state');
    if (uuid === null || state === null) return;
    // **The cancel settles on the id alone, before the hold's own guard**: the
    // pending mark is this prompt's whether or not a row is still held here -
    // a snapshot's release or a swap can have cleared the hold out from under
    // it - and skipping that test on the guard would leave the mark saying
    // "sending" forever. The store holds one send per seat, and the id is
    // what separates a second send from a first prompt's cancel; words could
    // not, since two sends of the same text compare equal.
    if (state === 'cancelled') {
      if (echoes.of(this.key)?.id === uuid) echoes.clear(this.key);
      this.waiting.delete(uuid);
      this.drained.delete(uuid);
      return;
    }
    const entry = this.waiting.get(uuid);
    const held = this.drained.get(uuid);
    if (entry === undefined && held === undefined) return;
    const dropped = state === 'refused' || state === 'discarded';
    const drawn = state === 'started' || state === 'completed';
    if (!dropped && !drawn) return;
    this.waiting.delete(uuid);
    this.drained.delete(uuid);
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
  /**
   * The same line twice in a row is one row: a repeated plan-limited turn
   * reports the same incident, and the page keeps one line for it - the
   * terminal's own upsert, at the grain this page draws.
   */
  private appendOnce(message: unknown): void {
    const last = this.held.turns.at(-1)?.messages.at(-1);
    if (last !== undefined && JSON.stringify(last) === JSON.stringify(message)) return;
    this.append(message);
  }

  private append(message: unknown): void {
    this.stream((held) => {
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
        if (last === undefined) {
          // No turn to join yet - and no row of its own to open. Held rather
          // than dropped (rule 25): the turn that will carry it is the one
          // still coming.
          this.unturned.push(message);
          return held;
        }
        const grown: Turn = { ...last, messages: [...last.messages, message], held: false };
        return this.answered({
          ...held,
          following: follow,
          turns: [...held.turns.slice(0, -1), grown],
        });
      }
      const taken = new Set(held.turns.map((turn) => turn.key));
      // The frames that waited for a turn ride ahead of the one that opened:
      // they arrived first, and a counter or a task fact reads in the turn it
      // was reported for.
      const riding = this.unturned;
      this.unturned = [];
      for (const frame of riding) this.ridden.add(frame);
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
        turns: [...held.turns, { key, messages: [...riding, message], live }],
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
