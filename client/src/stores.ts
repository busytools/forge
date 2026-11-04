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
   * up.
   */
  dropped: number;
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
  /** Replaces the snapshot and drops the updates it supersedes. */
  set(snapshot: unknown): void;
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
 * **The chat edge is appended last and the voice states only ever grow.** The
 * voice reader's cursor reads a front drop or a reset; replacing the edge can
 * shift its index by one and replay a voice state, never skip one, and a new
 * voice state always lengthens the list.
 */
function settledTail(updates: SessionUpdate[], update: SessionUpdate): SessionUpdate[] {
  const kept = keptVariant(update);
  if (kept === 'voice') return [...updates, update];
  if (kept === null) return updates;
  // The newest edge supersedes every one before it; the voice states stay.
  const withoutChat = updates.filter((waiting) => variantOf(waiting)[0] !== 'chat_appended');
  return [...withoutChat, update];
}

/** The tail cut to that rule, for a seat whose page has just gone. */
function compactedTail(updates: SessionUpdate[]): SessionUpdate[] {
  const voice = updates.filter((waiting) => variantOf(waiting)[0] === 'voice_mode_state');
  const chat = updates.filter(
    (waiting) => variantOf(waiting)[0] === 'chat_appended' && movesInFlight(chatFrame(waiting)),
  );
  const newest = chat[chat.length - 1];
  return newest === undefined ? voice : [...voice, newest];
}

function createStore(subject: Subject, state: StoreState = { kind: 'loading' }): Store {
  const inner = writable<StoreValue>({ snapshot: null, updates: [], state, dropped: 0 });
  // Audio is drained by the voice reader and never retained: a parsed
  // sentence is far larger than a state frame, and a listening session would
  // otherwise hold every one it has played.
  let audio: SessionUpdate[] = [];
  const MAX_AUDIO = 8;
  // Set when the last page drawing the seat goes: see `compact`.
  let compacted = false;
  return {
    subject,
    value: { subscribe: inner.subscribe },
    snapshot: () => get(inner).snapshot,
    updates: () => get(inner).updates,
    state: () => get(inner).state,
    dropped: () => get(inner).dropped,
    set: (snapshot) => inner.set({ snapshot, updates: [], state: { kind: 'ready' }, dropped: 0 }),
    compact: () => {
      if (!isSeat(subject) || compacted) return;
      compacted = true;
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
      inner.update((held) => {
        if (isSeat(subject) && replaces(update)) {
          return { snapshot: null, updates: [], state: { kind: 'loading' }, dropped: 0 };
        }
        const updates =
          isSeat(subject) && compacted
            ? settledTail(held.updates, update)
            : [...held.updates, update];
        const over = updates.length - MAX_UPDATES;
        return {
          snapshot: held.snapshot,
          updates: over > 0 ? updates.slice(over) : updates,
          state: held.state,
          dropped: held.dropped + Math.max(0, over),
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
