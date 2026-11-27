/**
 * One store per subscription, holding the snapshot a subject was answered
 * with and the updates that followed it.
 *
 * **A store holds both**, because a view redraws from either: a snapshot is
 * the whole subject and an update is one change to it, and which of the two a
 * page reads is the page's business.
 *
 * **A seat's store is keyed by the seat and holds one occupant's content**, so
 * a replacement drops what is there rather than leaving the next reader to
 * seed from the occupant that left (`REPLACES`). The key stays the seat
 * because the subscription does: it is counted, re-asked on a reconnect, and
 * routed to by the slot an update carries.
 *
 * **It also holds where the subscription got to**, because a refused
 * subscribe is an answer rather than a silence - the server sends one for a
 * seat nobody has started, so a page can say why instead of drawing an empty
 * snapshot as a broken page. `null` for `snapshot` cannot carry that: it
 * means loading, refused and genuinely empty at once.
 *
 * `svelte/store` rather than runes, because these are created per connection
 * and outlive any component: a rune outside a component needs a reactive root
 * and this has none.
 */

import { get, writable, type Readable } from 'svelte/store';

import type { Subject, SessionUpdate } from './protocol';
import { subjectKey } from './protocol';
import { movesInFlight, REPLACES, variantOf } from './session/apply';

/**
 * How many updates a store keeps before the oldest are dropped.
 *
 * A store holds what a page has not folded yet, and a page folds as it goes.
 * A connection stays open for a working day, so without a ceiling a busy seat
 * would grow this list for as long as the app is running.
 */
const MAX_UPDATES = 500;

/** Where a subscription got to. */
export type StoreState =
  /** Subscribed, and nothing has answered yet. */
  | { kind: 'loading' }
  /** Answered with a snapshot. */
  | { kind: 'ready' }
  /**
   * A patch skipped a version: the store holds its last read while one whole
   * read is asked for. The page draws the record it has and says it is
   * catching up; it never clears the store, so a gap never blanks a page.
   */
  | { kind: 'resyncing'; why: string }
  /** Refused, with the server's own reason. */
  | { kind: 'refused'; why: string };

/** What a `$store` read gives a component. */
export interface StoreValue {
  snapshot: unknown;
  updates: SessionUpdate[];
  state: StoreState;
  /**
   * How many updates the ceiling dropped off the front of `updates`, since
   * the snapshot it holds. A page folding them needs to know its tail is not
   * the whole story rather than finding out by arithmetic that does not add
   * up. A compacted tail is complete for its readers, so `compact()` resets
   * this to zero.
   */
  dropped: number;
  /**
   * The version the snapshot is at, `null` before a whole read answers.
   *
   * The base a patch is placed against: only the next number applies, and any
   * other is a gap.
   */
  version: number | null;
  /**
   * Patches this client could not place: one with no read behind it, one
   * whose shape it cannot read, or the patch that opened a gap while that gap
   * was being read. **Held rather than dropped** (rule 25): a page draws them
   * as themselves, so a client and a server out of step is visible instead of
   * silent.
   */
  unexpected: unknown[];
}

export interface Store {
  /** The subject this store is held under, as it was subscribed. */
  subject: Subject;
  /** A view subscribes to redraw; the value changes with the snapshot and the updates. */
  readonly value: Readable<StoreValue>;
  snapshot(): unknown;
  updates(): SessionUpdate[];
  state(): StoreState;
  dropped(): number;
  /** The version the snapshot is at, `null` before a whole read answers. */
  version(): number | null;
  /** Patches this client could not place, for a page to draw as themselves. */
  unexpected(): unknown[];
  /**
   * Replaces the snapshot and drops the updates it supersedes. `version` is
   * the number the record is at; absent from a server too old to carry one.
   */
  set(snapshot: unknown, version?: number): void;
  /**
   * The record a patch produced, written over the snapshot in place.
   *
   * The update tail is kept: a seat's frames and its record patch are one
   * page's material, and a patch does not supersede a frame the page has not
   * folded yet.
   */
  applied(version: number | null, snapshot: unknown): void;
  /**
   * A patch could not be placed: hold it for a page to draw (rule 25), and
   * mark the store as catching up while one whole read is asked for.
   */
  gap(why: string): void;
  /** One patch this client cannot place, held for the page to draw. */
  unplaced(data: unknown): void;
  /**
   * One update, held for a page to fold.
   *
   * A seat's store drops everything when the update replaces the seat's
   * record (`REPLACES`): what it holds is the previous occupant's, and a
   * reader seeding from it would draw that occupant's answer for the one
   * that arrived, until the seat's own answer lands.
   */
  push(update: SessionUpdate): void;
  /**
   * No page is drawing this seat: from here the tail keeps only what its two
   * readers fold - the newest chat edge and the voice states - rather than
   * every frame a busy seat streams while nobody reads.
   */
  compact(): void;
  /**
   * Whether the tail has been compacted to what its readers fold.
   *
   * It is then complete for them, so `runningAt` may read it even where the
   * ceiling's count says the tail lost its head.
   */
  compacted(): boolean;
  /**
   * A number that changes whenever the tail is reshaped rather than appended
   * to, so a cursor over it knows its indices no longer mean what they did.
   */
  generation(): number;
  /**
   * The `voice_audio` frames pushed since the last call, drained.
   *
   * A sentence is 0.3-1 MB as a parsed number array, and nothing but the
   * voice reader reads it: kept out of the retained tail, a listening
   * session holds only what has not played yet.
   */
  takeAudio(): SessionUpdate[];
  /** The server declined this subscription, with its own words for why. */
  refuse(why: string): void;
}

/**
 * Whether a subject is one seat's, rather than the fleet's or the pool's.
 *
 * The rule below is about a seat's record, and the home store holds a
 * subject of its own: a seat's swap is news for the home, not a reason to
 * drop what the home holds.
 */
function isSeat(subject: Subject): boolean {
  return typeof subject === 'object';
}

/** Whether one update replaces a seat's record rather than patching it. */
function replaces(update: SessionUpdate): boolean {
  const [name] = variantOf(update);
  return name !== null && REPLACES.includes(name);
}

/** The frame an update carries, for the seat tail's own question about it. */
function chatFrame(update: SessionUpdate): unknown {
  return (update as { chat_appended?: { msg?: unknown } }).chat_appended?.msg;
}

/**
 * What a seat's store is read for once no page holds the seat.
 *
 * `runningAt` (`chat/conversation.ts`) folds a turn's edges out of the chat
 * frames, and the voice control reads the listener count off `voice_mode_state`.
 * Nothing else has a reader: the chat draws from its own conversation.
 */
function keptVariant(update: SessionUpdate): 'chat' | 'voice' | null {
  const [name] = variantOf(update);
  if (name === 'chat_appended') return movesInFlight(chatFrame(update)) ? 'chat' : null;
  return name === 'voice_mode_state' ? 'voice' : null;
}

/**
 * A seat's tail while no page holds the seat.
 *
 * **Two readers, two frames.** `runningAt` moves only on a turn's edge, and
 * the voice control reads the newest listener count, so the tail holds the
 * newest edge and the newest voice state and nothing else. Both replace what
 * came before, which is a reshape rather than an append: the voice cursor
 * restarts on the store's generation (`voice/voice.svelte.ts`), so a replaced
 * state is replayed, not skipped.
 */
function settledTail(updates: SessionUpdate[], update: SessionUpdate): SessionUpdate[] {
  const kept = keptVariant(update);
  if (kept === 'voice') {
    const rest = updates.filter((waiting) => variantOf(waiting)[0] !== 'voice_mode_state');
    return [...rest, update];
  }
  if (kept === null) return updates;
  // The newest edge supersedes every one before it; the voice state stays.
  const withoutChat = updates.filter((waiting) => variantOf(waiting)[0] !== 'chat_appended');
  return [...withoutChat, update];
}

/** The tail cut to that rule, for a seat whose page has just gone. */
function compactedTail(updates: SessionUpdate[]): SessionUpdate[] {
  const voice = updates.filter((waiting) => variantOf(waiting)[0] === 'voice_mode_state');
  const chat = updates.filter(
    (waiting) => variantOf(waiting)[0] === 'chat_appended' && movesInFlight(chatFrame(waiting)),
  );
  const kept: SessionUpdate[] = [];
  const newestVoice = voice[voice.length - 1];
  const newestEdge = chat[chat.length - 1];
  if (newestVoice !== undefined) kept.push(newestVoice);
  if (newestEdge !== undefined) kept.push(newestEdge);
  return kept;
}

function createStore(subject: Subject, state: StoreState = { kind: 'loading' }): Store {
  const inner = writable<StoreValue>({
    snapshot: null,
    updates: [],
    state,
    dropped: 0,
    version: null,
    unexpected: [],
  });
  // Audio is drained by the voice reader and never retained: a parsed
  // sentence is far larger than a state frame, and a listening session would
  // otherwise hold every one it has played.
  let audio: SessionUpdate[] = [];
  const MAX_AUDIO = 8;
  // Set when the last page drawing the seat goes: see `compact`.
  let compacted = false;
  // Bumped whenever the tail is reshaped rather than appended to, so the
  // voice cursor knows the indices it holds no longer name its frames.
  let reshapes = 0;
  return {
    subject,
    value: { subscribe: inner.subscribe },
    snapshot: () => get(inner).snapshot,
    updates: () => get(inner).updates,
    state: () => get(inner).state,
    dropped: () => get(inner).dropped,
    version: () => get(inner).version,
    unexpected: () => get(inner).unexpected,
    compacted: () => compacted,
    generation: () => reshapes,
    set: (snapshot, version) =>
      inner.set({
        snapshot,
        updates: [],
        state: { kind: 'ready' },
        dropped: 0,
        version: typeof version === 'number' ? version : null,
        unexpected: [],
      }),
    applied: (version, snapshot) =>
      inner.update((held) => ({
        ...held,
        snapshot,
        state: { kind: 'ready' },
        // A record subject's patch carries its next number; a seat's does
        // not, and its version stays whatever the whole read said.
        version: version ?? held.version,
      })),
    gap: (why) => inner.update((held) => ({ ...held, state: { kind: 'resyncing', why } })),
    unplaced: (data) =>
      inner.update((held) => ({
        ...held,
        // Bounded: an out-of-step pair must not grow this list for the life
        // of the connection.
        unexpected: [...held.unexpected, data].slice(-10),
      })),
    compact: () => {
      if (!isSeat(subject) || compacted) return;
      compacted = true;
      reshapes += 1;
      inner.update((held) => ({
        ...held,
        updates: compactedTail(held.updates),
        // The tail is complete for its readers now. Keeping the ceiling's
        // count would make `runningAt` refuse the tail it can still answer
        // from and read a stale snapshot instead.
        dropped: 0,
      }));
    },
    push: (update) => {
      if (variantOf(update)[0] === 'voice_audio') {
        audio.push(update);
        if (audio.length > MAX_AUDIO) audio.splice(0, audio.length - MAX_AUDIO);
        // The control pumps on a store change; the retained value is the
        // same, so a page folding it draws nothing new.
        inner.update((held) => ({ ...held }));
        return;
      }
      const replacing = isSeat(subject) && compacted;
      if (replacing && keptVariant(update) !== null) reshapes += 1;
      inner.update((held) => {
        if (isSeat(subject) && replaces(update)) {
          // A new occupant's stream starts over: the number the next read
          // carries is the new epoch's, so the old base goes with the record.
          return {
            snapshot: null,
            updates: [],
            state: { kind: 'loading' },
            dropped: 0,
            version: null,
            unexpected: [],
          };
        }
        const updates = replacing ? settledTail(held.updates, update) : [...held.updates, update];
        const over = updates.length - MAX_UPDATES;
        return {
          snapshot: held.snapshot,
          updates: over > 0 ? updates.slice(over) : updates,
          state: held.state,
          dropped: held.dropped + Math.max(0, over),
          version: held.version,
          unexpected: held.unexpected,
        };
      });
    },
    takeAudio: () => {
      const taken = audio;
      audio = [];
      return taken;
    },
    refuse: (why) =>
      inner.update((held) => ({ ...held, updates: [], state: { kind: 'refused', why } })),
  };
}

/** The stores one connection holds, by subject. */
export class Stores {
  private readonly held = new Map<string, { store: Store; subscriptions: number }>();

  /**
   * A store for a subject, adding a subscription to it.
   *
   * Counted rather than replaced: a second subscribe to one subject is a
   * second subscription, and one unsubscribe must not take the seat out of
   * the set a view is still watching.
   */
  open(subject: Subject): Store {
    const key = subjectKey(subject);
    const existing = this.held.get(key);
    if (existing) {
      existing.subscriptions += 1;
      return existing.store;
    }
    const store = createStore(subject);
    this.held.set(key, { store, subscriptions: 1 });
    return store;
  }

  /**
   * A store that will never be subscribed, for a connection that is already
   * closed. Handing back a live-looking store would promise a snapshot no
   * reconnect would ever bring, because nothing replays a subscribe made
   * after the socket went.
   */
  refused(subject: Subject, why: string): Store {
    return createStore(subject, { kind: 'refused', why });
  }

  /**
   * One subscription to a subject, dropped. The store goes with the last
   * one, and answers `true` when it did.
   */
  close(subject: Subject): boolean {
    const key = subjectKey(subject);
    const existing = this.held.get(key);
    if (!existing) return false;
    existing.subscriptions -= 1;
    if (existing.subscriptions > 0) return false;
    this.held.delete(key);
    return true;
  }

  get(subject: Subject): Store | undefined {
    return this.held.get(subjectKey(subject))?.store;
  }

  /**
   * How many subscriptions this connection holds for a subject, which is how
   * many times the server must be asked for it again after a drop: one
   * unsubscribe drops one of its entries, so a reconnect that re-asks once
   * for a subject held twice leaves them out of step.
   */
  count(subject: Subject): number {
    return this.held.get(subjectKey(subject))?.subscriptions ?? 0;
  }

  /** The store held at a `subjectKey`, for a caller that already has the key. */
  byKey(key: string): Store | undefined {
    return this.held.get(key)?.store;
  }

  /**
   * Every subject this connection currently holds, for a caller that has to
   * sweep the set rather than address one by key.
   */
  subjects(): Subject[] {
    return Array.from(this.held.values(), ({ store }) => store.subject);
  }
}
