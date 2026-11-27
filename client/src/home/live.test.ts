import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { ServerMessage, Subject } from '../protocol';
import { PROTOCOL_VERSION } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import { Stores } from '../stores';
import type { Task } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import { railFooter } from '../session/view';
import { homeWire } from '../dev/fixture.data';
import { COALESCE_MS, rereadHome, watchHome, type HomeRead } from './live';
import { homeView } from './view';

const HOME: Subject = 'home';
const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

/** An update a home subscriber is sent, which is what it answers with a read. */
const UPDATE: ServerMessage = { kind: 'update', update: { turn_cancelled: { key: LEAD } } };

/** The account the pool fixture names, which the rail chip resolves to. */
const POOL_ACCOUNT = 'OpenRouter';

/** One usage row for `POOL_ACCOUNT` at `balance`. */
function poolUsage(balance: number) {
  return {
    display_name: POOL_ACCOUNT,
    snapshot: {
      source: 'OpenRouterKey',
      fetched_at: { secs_since_epoch: 1, nanos_since_epoch: 0 },
      five_hour: null,
      seven_day: null,
      seven_day_opus: null,
      seven_day_sonnet: null,
      extra_usage: null,
      spend: {
        daily: 1.5,
        weekly: 2,
        monthly: 3,
        limit: 10,
        limit_remaining: 8,
        limit_reset: null,
        expires_at: null,
      },
      balance,
    },
  };
}

/** The pool's non-usage facts: one ready account, the listener bound. */
const POOL_FACTS = {
  loading: [
    {
      display_name: POOL_ACCOUNT,
      state: 'ready',
      last_error: null,
      retry_after: null,
      auth: 'oauth',
    },
  ],
  all_loaded: true,
  gateway: { ready: true, port: 8787, bind_error: null },
};

/** A pool as a frame carries it: the facts above, balance 7.25. */
const POOL = { ...POOL_FACTS, usage: [poolUsage(7.25)] };

/** The same facts with an older usage row, as the snapshot before the frame. */
const POOL_OLD = { ...POOL_FACTS, usage: [poolUsage(7.0)] };

/** A home whose one project chips `POOL_ACCOUNT`, so the rail footer resolves it. */
const POOL_HOME = {
  agents: [],
  projects: [
    {
      project: {
        key: 'k',
        name: 'proj',
        org: 'TestOrg',
        path: '/p',
        display_path: '~/p',
        accounts: [POOL_ACCOUNT],
        fallback_accounts: [],
        has_model: true,
        sessions: [],
      },
      work: { branch: 'main', changed: 0, gate: 'in_repo' },
      would_bind: true,
      chip: { account_name: POOL_ACCOUNT, state: 'normal' },
    },
  ],
  unseen: [],
  accounts: POOL,
  dictate: { snapshot: { models: [] } },
};

/** The same home with the older usage row; its facts match `POOL`. */
const POOL_HOME_OLD = { ...POOL_HOME, accounts: POOL_OLD };

/**
 * The fixture's seat, idle with no ask, so the lifecycle fold owns its mark.
 * `stateOf` promotes a pending ask and a background task over the lifecycle,
 * so the cases that move the lifecycle clear both.
 */
const IDLE_AGENT = {
  slot: LEAD,
  label: 'lead',
  lifecycle: 'Idle',
  heal_attempt: null,
  heal_attempts: null,
  has_background_work: false,
  pending: null,
  pending_depth: 0,
  pending_seq: 0,
  last_activity: null,
  reason: null,
  failed_turn: null,
  died: false,
  tree: null,
  work: { branch: null, changed: null, gate: 'gone' },
};

/** A home whose one seat is idle, for the folds that move its mark. */
function idleHome() {
  return { ...homeWire, agents: [{ ...IDLE_AGENT }] };
}

/**
 * One prompt-lifecycle frame for the fixture's seat, spelled the way these
 * cases reason about it: a word this build knows maps into the wire's one
 * vocabulary, and anything else crosses as itself so an unknown shape keeps
 * the read.
 */
function life(state: string, uuid = 'p1'): ServerMessage {
  const wire: unknown =
    state === 'queued'
      ? 'queued'
      : state === 'started' || state === 'completed'
        ? 'sent'
        : state === 'cancelled' || state === 'discarded' || state === 'refused'
          ? { not_sent: { reason: `the CLI ${state} it before it ran` } }
          : state;
  return {
    kind: 'update',
    update: { prompt_lifecycle: { key: LEAD, uuid, state: wire, source: 'cli' } },
  };
}

/** One CLI background-registry frame for the fixture's seat. */
function background(tasks: unknown[]): ServerMessage {
  return {
    kind: 'update',
    update: {
      chat_appended: {
        key: LEAD,
        msg: { type: 'system', subtype: 'background_tasks_changed', tasks },
      },
    },
  } as unknown as ServerMessage;
}

/**
 * A connection a test drives by hand: what the home asked it to do, and a way
 * to hand it a frame.
 *
 * The stores are the real ones, because a subscription's lifecycle is what
 * this fakes around rather than anything about a store.
 */
function fakeConnection() {
  const stores = new Stores();
  const subscribed: Subject[] = [];
  /** The options each subscribe went out with, which the role rides. */
  const options: { answering?: boolean; browser?: boolean }[] = [];
  const unsubscribed: Subject[] = [];
  const refreshed: Subject[] = [];
  const messages = new Set<(message: ServerMessage) => void>();
  const statuses = new Set<(status: ConnectionStatus) => void>();

  const connection: Connection = {
    subscribe(what, chosen) {
      subscribed.push(what);
      options.push(chosen ?? {});
      return stores.open(what);
    },
    unsubscribe(what) {
      unsubscribed.push(what);
      return stores.close(what);
    },
    holds: (what) => stores.get(what) !== undefined,
    refresh(what) {
      refreshed.push(what);
      return true;
    },
    showing() {},
    relist() {},
    declare() {},
    release() {},
    browserInput: () => false,
    onBrowserFrame: () => () => {},
    onMessage(fn) {
      messages.add(fn);
      return () => {
        messages.delete(fn);
      };
    },
    onStatus(fn) {
      statuses.add(fn);
      return () => {
        statuses.delete(fn);
      };
    },
    dispatch: () => Promise.resolve(null),
    more: () => false,
    commitDiff: () => false,
    fileContent: () => false,
    readDocument: () => false,
    seatRange: () => false,
    frame: () => false,
    store: () => undefined,
    settings: () => null,
    skew: () => null,
    serverProtocol: () => PROTOCOL_VERSION,
    status: () => 'open',
    probe: () => {},
    close: () => {},
  };

  return {
    connection,
    stores,
    subscribed,
    options,
    unsubscribed,
    refreshed,
    /** Everything still attached to the connection. */
    listening: () => messages.size + statuses.size,
    /** One frame as the server sent it, into whatever is still listening. */
    arrive(message: ServerMessage) {
      for (const fn of [...messages]) fn(message);
    },
    /** The connection's status moved, as the socket reports it. */
    status(next: ConnectionStatus) {
      for (const fn of [...statuses]) fn(next);
    },
  };
}

/**
 * Drive the coalescing beat, which is the only clock these cases wait on.
 *
 * The beat is the store's own (`COALESCE_MS`), so it fires exactly when the
 * store would fire it, and no wall clock is left for a loaded runner to
 * outrun.
 */
async function settle(): Promise<void> {
  await vi.advanceTimersByTimeAsync(COALESCE_MS);
}

/**
 * The floor between two whole reads, which the product promises: a stream of
 * frames buys at most one read per floor, however fast it arrives (#3060).
 *
 * Spelled out rather than imported, so the cases state the cadence the
 * product promises rather than reading it back off the code they check.
 */
const FLOOR_MS = 2_000;

/** Advance past the floor: the wait a read owes while one has just gone. */
async function settleFloor(): Promise<void> {
  await vi.advanceTimersByTimeAsync(FLOOR_MS);
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe('the home over a connection', () => {
  /**
   * **A return asks through the page's guards.** One read at a time: an ask
   * while one is in flight would queue a second whole encode behind it, and a
   * subject nobody watches is not asked for at all.
   */
  it('a return reads through the page, one at a time', () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});

    rereadHome(forge.connection);
    expect(forge.refreshed, 'the return asks for the home').toEqual([HOME]);
    rereadHome(forge.connection);
    expect(forge.refreshed, 'a read in flight is not asked again').toEqual([HOME]);

    forge.arrive({ kind: 'snapshot', subject: HOME, data: {} });
    rereadHome(forge.connection);
    expect(forge.refreshed, 'the answered read frees the next').toEqual([HOME, HOME]);

    stop();
    forge.refreshed.length = 0;
    rereadHome(forge.connection);
    expect(forge.refreshed, 'an unwatched home is not asked for').toEqual([]);
  });

  /**
   * **The connection declares itself answerable, usually on its first
   * subscribe.** The role belongs to the connection and only ever rises, and
   * this subscribe is usually the first the app makes (/models gets there
   * first on a deep link; a seat page waits on the home's own snapshot), so
   * declaring it here is what a page away from any seat has. It is a
   * safeguard rather than the ask's own cure - another subscriber may be the
   * one that answers - and the case it covers is a serve with no other
   * answerer anywhere.
   */
  it('declares the connection answerable, usually on its first subscribe', () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});

    expect(forge.subscribed, 'the home was not subscribed').toEqual([HOME]);
    expect(forge.options[0]?.answering, 'the connection declares nothing to answer with').toBe(
      true,
    );

    stop();
  });

  /**
   * **An update that lands mid-read is not dropped (#1885).** The home
   * coalesces its reads behind one flag, and the answer in flight was encoded
   * BEFORE the update arrived - so skipping the update leaves the page drawing
   * the state it asked about until something else moves. That is the wait Ved
   * met: an ask answered, and the row still sitting under needs-you.
   */
  it('counts a task frame into the fleet row without a read', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe(($next) => {
      seen.push($next);
    });
    forge.stores.get(HOME)?.set(homeWire);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: homeWire });

    forge.arrive({
      kind: 'update',
      update: {
        tasks_changed: {
          key: { org: 'TestOrg', project: 'proj', label: 'lead' },
          tasks: [
            { id: 't1', project_name: 'proj', status: 'pending', owner: null },
            { id: 't2', project_name: 'proj', status: 'pending', owner: null },
            {
              id: 't3',
              project_name: 'proj',
              status: 'waiting',
              owner: null,
              waiting_on: { kind: 'decision', detail: null, on: null, verification: false },
            },
          ],
        },
      },
    });
    await settle();
    expect(forge.refreshed, 'a task write asked for a whole home read').toEqual([]);
    const row = seen.at(-1)?.wire?.fleet.find((entry) => entry.project === 'proj');
    expect(row?.queue, 'the frame did not move the ready count').toBe(2);
    expect(row?.waiting_on_user, 'the decision wait was not counted').toBe(1);
    expect(row?.rows, 'the row count is the set').toBe(3);
    expect(row?.live_workers, 'the server-derived figure moved').toBe(1);
    stop();
  });

  it('asks again for an update that lands while a read is in flight', async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});

    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'the first update did not ask for a read').toEqual(['home']);

    // The read is in flight: no answer for it has arrived yet.
    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'a second read ran while the first was in flight').toEqual(['home']);

    // Its answer lands, and the update that arrived meanwhile is asked for -
    // at the floor, because a whole read has just gone.
    forge.arrive({ kind: 'snapshot', subject: HOME, data: null });
    await settle();
    expect(forge.refreshed, 'a second read went inside the floor').toEqual(['home']);
    await settleFloor();
    expect(forge.refreshed, 'the update that landed mid-read was dropped').toEqual([
      'home',
      'home',
    ]);
    stop();
  });

  /**
   * **A refused read settles nothing the update asked for.** The store is
   * refused before the frame reaches this listener, so no read can go out
   * from the error arm at all - the update's mark must survive the refusal
   * and be spent by the snapshot that re-establishes the subject (#2559).
   */
  /**
   * **A stream of fleet news buys one whole read per floor, not one per
   * frame.** The coalescing beat folds a burst, but a busy fleet's frames
   * arrive closer together than an answer comes back, so the beat alone fires
   * as fast as the round trip: measured on a live fleet at 1-3 whole reads a
   * second, ~195 KB each (#3060).
   */
  it('bounds a stream of fleet news to one read per floor', async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});

    // Twenty seconds of news, one frame a second, each read answered as the
    // server would answer it.
    for (let second = 0; second < 20; second += 1) {
      const before = forge.refreshed.length;
      forge.arrive(UPDATE);
      await vi.advanceTimersByTimeAsync(1_000);
      if (forge.refreshed.length > before) {
        forge.arrive({ kind: 'snapshot', subject: HOME, data: POOL_HOME });
      }
    }

    expect(
      forge.refreshed.length,
      'the stream bought more than one read per floor',
    ).toBeLessThanOrEqual(Math.ceil(20_000 / FLOOR_MS) + 1);
    expect(forge.refreshed.length, 'the stream stopped being read at all').toBeGreaterThan(1);
    stop();
  });

  /**
   * **The floor delays a read; it never drops one.** News that lands while
   * the floor holds is read when it lifts, because the read in hand was
   * encoded before it - the same reason the mid-read update is kept (#1885).
   */
  it('reads the news that arrives inside the floor once it lifts', async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});

    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'the first update did not ask for a read').toEqual([HOME]);

    // The read is answered and the next frame lands at once.
    forge.arrive({ kind: 'snapshot', subject: HOME, data: POOL_HOME });
    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'a second read went inside the floor').toEqual([HOME]);

    await settleFloor();
    expect(forge.refreshed, 'the news behind the floor was dropped').toEqual([HOME, HOME]);
    stop();
  });

  /**
   * **A return's own read starts the floor too.** It asks directly rather
   * than through the beat, so without the floor a frame landing right behind
   * its answer would buy a second whole encode inside the beat - two reads
   * where the return already answered.
   */
  it('a return starts the floor for the frame behind it', async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});

    rereadHome(forge.connection);
    expect(forge.refreshed, 'the return did not ask for a read').toEqual([HOME]);

    forge.arrive({ kind: 'snapshot', subject: HOME, data: POOL_HOME });
    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'a frame right behind a return read again at once').toEqual([HOME]);

    await settleFloor();
    expect(forge.refreshed, 'the frame behind the floor was dropped').toEqual([HOME, HOME]);
    stop();
  });

  it('keeps the mid-read update for the snapshot that re-establishes a refused subject', async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});

    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'the first update did not ask for a read').toEqual(['home']);

    // The transport refuses the store first, then delivers the frame - the
    // order `socket.ts` keeps, and the reason a re-ask here could not fire.
    forge.stores.get('home')?.refuse('this forge holds no home');
    forge.arrive(UPDATE);
    forge.arrive({
      kind: 'error',
      what: 'subscribe',
      why: 'this forge holds no home',
      seat: null,
      subject: 'home',
    });
    await settle();
    expect(forge.refreshed, 'no read can go out while the subject is refused').toEqual(['home']);

    // The subject comes back: its snapshot is newer than the update, and the
    // mark it spends asks for the read the refusal could not - at the floor.
    forge.stores.get('home')?.set({});
    forge.arrive({ kind: 'snapshot', subject: HOME, data: {} });
    await settleFloor();
    expect(forge.refreshed, 'the update that landed mid-read was dropped').toEqual([
      'home',
      'home',
    ]);
    stop();
  });

  /**
   * **An unrelated refusal does not settle the home's read.** The error arm
   * acts only on its own subject: another subject's refusal must leave the
   * read in flight, so a later update is not asked for twice while the first
   * answer is still owed.
   */
  it("does not settle the home's in-flight read on another subject's refusal", async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});

    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'the first update did not ask for a read').toEqual(['home']);

    forge.arrive({
      kind: 'error',
      what: 'subscribe',
      why: 'this forge holds no board',
      seat: null,
      subject: 'board',
    });
    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, "another subject's refusal freed the home's ask").toEqual(['home']);

    forge.arrive({ kind: 'snapshot', subject: HOME, data: {} });
    await settleFloor();
    expect(forge.refreshed, 'and the answer spends the update behind it').toEqual(['home', 'home']);
    stop();
  });

  /**
   * **A return inside the coalesce beat is the read the update wanted.** The
   * timer fires after a return has already asked; queueing a second whole
   * encode behind it lets the first answer clear the flag mid-flight and a
   * third read follow.
   */
  it('a return inside the coalesce beat does not add a read', async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});

    forge.arrive(UPDATE);
    rereadHome(forge.connection);
    await settle();
    expect(forge.refreshed, 'the return and the beat asked twice').toEqual(['home']);
    stop();
  });

  /**
   * **A refusal another subject owns is not the home's.** The board's refused
   * subscribe arrives as an `error` too; taking it for the home's would read
   * the home store and, with an update behind it, ask for a home read that
   * nothing asked for (#2517).
   */
  it("does not answer another subject's refusal with a home read", async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});

    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'the first update did not ask for a read').toEqual(['home']);

    // An update lands behind the read in flight, then the board is refused.
    forge.arrive(UPDATE);
    forge.arrive({
      kind: 'error',
      what: 'subscribe',
      why: 'this forge holds no board',
      seat: null,
      subject: 'board',
    });
    await settle();
    expect(forge.refreshed, "another subject's refusal asked the home to read").toEqual(['home']);
    stop();
  });

  /**
   * **A prompt's life is folded into the drawn row, not answered with a
   * read.** The frame names the prompt and its state, and the row's lifecycle
   * is what a per-seat set of prompt uuids reproduces: `queued` adds the
   * uuid, `started` keeps it, a terminal removes it. A repeat of one uuid is
   * no second prompt. **A fold may only move a row to Running**: the terminal
   * that empties the set keeps the read, because the drawn Running may count
   * a prompt this page never saw. The set is re-based by every snapshot
   * answer and cleared by a socket drop, so a reconnect starts from the
   * snapshot.
   */
  it("folds a prompt's life into the drawn row without a read", async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe(($next) => {
      seen.push($next);
    });
    const wire = idleHome();
    forge.stores.get(HOME)?.set(wire);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: wire });

    forge.arrive(life('queued'));
    // The same prompt announced again is no second one.
    forge.arrive(life('queued'));
    await settle();
    expect(forge.refreshed, "a prompt's queue asked for a whole read").toEqual([]);
    expect(seen.at(-1)?.wire?.agents[0]?.lifecycle, 'the queued prompt did not start the row').toBe(
      'Running',
    );

    // **`sent` is the terminal now**: the prompt left the queue when the CLI
    // took it, and the turn it started is the frames' to state. The last
    // prompt leaving is the one terminal a fold cannot take on its own - the
    // drawn Running may count a prompt this page never saw, so Idle is the
    // read's to state and the row keeps Running until it lands.
    forge.arrive(life('started'));
    await settle();
    expect(forge.refreshed, 'the last prompt leaving asked for the read Idle needs').toEqual([
      HOME,
    ]);
    expect(seen.at(-1)?.wire?.agents[0]?.lifecycle).toBe('Running');

    // A terminal for a prompt this page no longer counts keeps the read too:
    // the state is still the read's to settle.
    forge.arrive(life('completed'));
    await settle();
    expect(forge.refreshed, 'the repeat is answered by the read already owed').toEqual([HOME]);
    expect(
      seen.at(-1)?.wire?.agents[0]?.lifecycle,
      'the fold moved the row to Idle on its own',
    ).toBe('Running');
    stop();
  });

  /**
   * **A snapshot answer must not clobber a count the frames built.** A prompt
   * queued behind a running turn is a second uuid; an answer that lands while
   * both are live keeps it, so the running prompt's terminal does not publish
   * Idle under a queued turn. The second prompt's own terminal is the empty
   * set and keeps the read. A reconnect forgets the set, so the next terminal
   * names a prompt this page never counted and keeps the read too.
   */
  it('keeps a second queued prompt across a snapshot, and forgets it on a reconnect', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe(($next) => {
      seen.push($next);
    });
    const wire = idleHome();
    forge.stores.get(HOME)?.set(wire);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: wire });

    forge.arrive(life('queued', 'p1'));
    forge.arrive(life('queued', 'p2'));
    await settle();
    expect(forge.refreshed, 'two queued prompts asked for a read').toEqual([]);

    // An answer lands while both are live and the server states the seat is
    // running: the set must survive it.
    const running = { ...wire, agents: [{ ...IDLE_AGENT, lifecycle: 'Running' }] };
    forge.stores.get(HOME)?.set(running);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: running });
    forge.arrive(life('completed', 'p1'));
    await settle();
    expect(forge.refreshed, 'the second prompt went uncounted after a snapshot').toEqual([]);
    expect(
      seen.at(-1)?.wire?.agents[0]?.lifecycle,
      'the row went idle while a second prompt was queued',
    ).toBe('Running');

    forge.arrive(life('started', 'p2'));
    await settle();
    expect(forge.refreshed, 'the second prompt leaving emptied the set').toEqual([HOME]);
    forge.arrive(life('completed', 'p2'));
    await settle();
    expect(forge.refreshed, 'a terminal for an uncounted prompt keeps the read').toEqual([HOME]);
    expect(seen.at(-1)?.wire?.agents[0]?.lifecycle).toBe('Running');

    // A reconnect forgets every set: a terminal for a prompt the page never
    // counted keeps the read rather than folding a stale count.
    forge.arrive(life('queued', 'p3'));
    await settle();
    forge.status('connecting');
    forge.status('open');
    const rebased = { ...wire, agents: [{ ...IDLE_AGENT, lifecycle: 'Running' }] };
    forge.stores.get(HOME)?.set(rebased);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: rebased });
    await settle();
    forge.arrive(life('completed', 'p3'));
    await settleFloor();
    expect(
      forge.refreshed,
      'a terminal for a pre-reconnect prompt folded a count the page no longer held',
    ).toEqual([HOME, HOME]);
    stop();
  });

  /**
   * **A row a read drew Running keeps the read when a prompt this page counted
   * settles.** A page that attached mid-turn has no uuids: the set empties
   * when its own prompt ends, while the turn the snapshot drew is still
   * running. The empty set is therefore no proof of Idle, and the fold must
   * leave the mark to the read.
   */
  it('keeps the read when the empty set sits under a snapshot-drawn Running', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe(($next) => {
      seen.push($next);
    });
    // The page attached mid-turn: the drawn Running is the server's, and the
    // page has seen none of its prompts.
    const running = { ...idleHome(), agents: [{ ...IDLE_AGENT, lifecycle: 'Running' }] };
    forge.stores.get(HOME)?.set(running);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: running });

    forge.arrive(life('queued', 'p7'));
    await settle();
    expect(forge.refreshed, 'the queued prompt asked for a read').toEqual([]);
    expect(seen.at(-1)?.wire?.agents[0]?.lifecycle).toBe('Running');

    forge.arrive(life('cancelled', 'p7'));
    await settle();
    expect(forge.refreshed, 'the empty set folded Idle under the turn the snapshot drew').toEqual([
      HOME,
    ]);
    expect(
      seen.at(-1)?.wire?.agents[0]?.lifecycle,
      'the row went idle while the original turn was still running',
    ).toBe('Running');
    stop();
  });

  /**
   * A prompt's life with no drawn row to fold into is the first read still
   * owed: the frame has nowhere to land, so the read is the failure
   * direction.
   */
  it("reads for a prompt's life when the row it names is not drawn", async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});
    forge.arrive(life('started'));
    await settle();
    expect(forge.refreshed, 'a frame with no drawn row to fold into bought no read').toEqual([
      HOME,
    ]);
    stop();
  });

  /**
   * **A successful result still buys the read that moves `unseen`.** The
   * completion is the one home value no frame carries whole: the mark is the
   * server's cross-client fact (no client was showing the seat), so a fold
   * here would either invent it or lose it. This pins the residual the folds
   * must not swallow.
   */
  it('still reads for a successful result, the completion the home cannot fold', async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});
    const wire = idleHome();
    forge.stores.get(HOME)?.set(wire);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: wire });

    forge.arrive({
      kind: 'update',
      update: {
        chat_appended: {
          key: LEAD,
          msg: { type: 'result', subtype: 'success', is_error: false },
        },
      },
    } as unknown as ServerMessage);
    await settle();
    expect(
      forge.refreshed,
      'a successful result stopped asking for the read that moves unseen',
    ).toEqual([HOME]);
    stop();
  });

  /**
   * **Every fold keeps its read when the value cannot be reproduced.** These
   * are the four refusals the failure direction names: a seat the drawn home
   * does not hold, a row another source owns, a prompt state this build has
   * not seen, and a vault the home states none of.
   */
  it('keeps the read for a frame the drawn home cannot reproduce', async () => {
    // A seat the drawn home does not hold.
    const absent = fakeConnection();
    const stopAbsent = watchHome(absent.connection).subscribe(() => {});
    const noSeat = { ...idleHome(), agents: [] };
    absent.stores.get(HOME)?.set(noSeat);
    absent.arrive({ kind: 'snapshot', subject: HOME, data: noSeat });
    absent.arrive(life('queued'));
    await settle();
    expect(absent.refreshed, 'a seat the drawn home does not hold was folded into nothing').toEqual(
      [HOME],
    );
    stopAbsent();

    // A row in a state another source owns.
    const owned = fakeConnection();
    const stopOwned = watchHome(owned.connection).subscribe(() => {});
    const attention = { ...idleHome(), agents: [{ ...IDLE_AGENT, lifecycle: 'Attention' }] };
    owned.stores.get(HOME)?.set(attention);
    owned.arrive({ kind: 'snapshot', subject: HOME, data: attention });
    owned.arrive(life('queued'));
    await settle();
    expect(owned.refreshed, 'a row an ask owns was overwritten by the fold').toEqual([HOME]);
    stopOwned();

    // A prompt state this build has not seen.
    const unknown = fakeConnection();
    const stopUnknown = watchHome(unknown.connection).subscribe(() => {});
    const idle = idleHome();
    unknown.stores.get(HOME)?.set(idle);
    unknown.arrive({ kind: 'snapshot', subject: HOME, data: idle });
    unknown.arrive(life('finished'));
    await settle();
    expect(unknown.refreshed, 'an unknown prompt state was guessed instead of read').toEqual([
      HOME,
    ]);
    stopUnknown();

    // A vault the home states none of.
    const noVault = fakeConnection();
    const stopNoVault = watchHome(noVault.connection).subscribe(() => {});
    const bare = { ...idleHome(), vault: null };
    noVault.stores.get(HOME)?.set(bare);
    noVault.arrive({ kind: 'snapshot', subject: HOME, data: bare });
    noVault.arrive({ kind: 'update', update: { vault_lock_changed: { locked: false } } });
    await settle();
    expect(noVault.refreshed, 'a lock for a vault the home states none of was folded').toEqual([
      HOME,
    ]);
    stopNoVault();
  });

  /**
   * The CLI's background registry moves `has_background_work`, and the frame
   * carries the whole registry, so the row is drawn from it rather than
   * answered with a read. An empty registry clears the promotion the same
   * way it raised it.
   */
  it('folds the CLI background registry into the drawn row without a read', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe(($next) => {
      seen.push($next);
    });
    const wire = idleHome();
    forge.stores.get(HOME)?.set(wire);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: wire });

    forge.arrive(background([{ task_id: 'b1' }]));
    await settle();
    expect(forge.refreshed, 'a background task starting asked for a whole read').toEqual([]);
    expect(seen.at(-1)?.wire?.agents[0]?.has_background_work, 'the registry did not land').toBe(
      true,
    );

    forge.arrive(background([]));
    await settle();
    expect(forge.refreshed, 'a drained registry asked for a whole read').toEqual([]);
    expect(
      seen.at(-1)?.wire?.agents[0]?.has_background_work,
      'the drained registry did not clear',
    ).toBe(false);
    stop();
  });

  /**
   * **The core's held status and its fatal are the snapshot's own fields**,
   * and the frames carry them whole, so the banner and the exit line are
   * drawn from the frame. The service report is one-shot (the probe never
   * clears it), and a fatal is the last word there is.
   */
  it("folds the core's service report and fatal without a read", async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe(($next) => {
      seen.push($next);
    });
    const wire = idleHome();
    forge.stores.get(HOME)?.set(wire);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: wire });

    forge.arrive({
      kind: 'update',
      update: { service_status: { severity: 'error', message: 'the core is down' } },
    });
    await settle();
    expect(forge.refreshed, "the core's status asked for a whole read").toEqual([]);
    expect(seen.at(-1)?.wire?.service_status, 'the status did not land').toEqual({
      severity: 'error',
      message: 'the core is down',
    });
    expect(seen.at(-1)?.report, 'the update line did not land').toEqual({
      severity: 'error',
      message: 'the core is down',
    });

    forge.arrive({
      kind: 'update',
      update: { fatal_error: { error: 'AppError', message: 'forge is going away' } },
    });
    await settle();
    expect(forge.refreshed, "the core's fatal asked for a whole read").toEqual([]);
    expect(seen.at(-1)?.wire?.fatal_error, 'the fatal did not land').toBe('forge is going away');
    stop();
  });

  /**
   * The vault's lock and the device enrolments are fields the frames carry
   * whole (the lock's idle bound is the config's and stays), so the settings
   * card redraws without a read. A vault the snapshot says this machine has
   * none of has nowhere to put the lock: the read is the failure direction.
   */
  it('folds a vault lock and the enrolment list without a read', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe(($next) => {
      seen.push($next);
    });
    const wire = {
      ...idleHome(),
      vault: { locked: true, idle_minutes: 5 },
      biometric: { outcome: 'read', value: [] },
    };
    forge.stores.get(HOME)?.set(wire);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: wire });

    forge.arrive({ kind: 'update', update: { vault_lock_changed: { locked: false } } });
    await settle();
    expect(forge.refreshed, 'a vault lock asked for a whole read').toEqual([]);
    expect(seen.at(-1)?.wire?.vault, 'the lock did not land').toEqual({
      locked: false,
      idle_minutes: 5,
    });

    forge.arrive({
      kind: 'update',
      update: {
        biometric_enrolments_changed: {
          enrolments: [
            {
              id: 'd1',
              device: 'Mac Studio',
              created_at: { secs_since_epoch: 1, nanos_since_epoch: 0 },
              purposes: [],
            },
          ],
        },
      },
    });
    await settle();
    expect(forge.refreshed, 'an enrolment list asked for a whole read').toEqual([]);
    const held = seen.at(-1)?.wire?.biometric;
    expect(held?.outcome).toBe('read');
    expect(held?.value).toHaveLength(1);
    stop();
  });

  /**
   * **A re-announced task set is no read** (#2540). The chase used to send
   * every project every 30 s with the set unchanged, and a home read for
   * that was a loop with no news; only a moved set asks.
   */
  it('skips a task set that has not moved', async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});
    const task = {
      id: 't1',
      project_name: 'proj',
      subject: 'a row',
      status: 'pending',
    } as unknown as Task;
    const same: ServerMessage = {
      kind: 'update',
      update: { tasks_changed: { key: LEAD, tasks: [task] } },
    };
    const moved: ServerMessage = {
      kind: 'update',
      update: { tasks_changed: { key: LEAD, tasks: [{ ...task, status: 'completed' }] } },
    };

    forge.arrive(same);
    await settle();
    expect(forge.refreshed, 'the first task set asked for a read').toEqual([HOME]);

    // The read in hand answers, so a later moved set can ask again.
    forge.arrive({ kind: 'snapshot', subject: HOME, data: {} });
    await settle();

    // A fresh deep-equal object, not the same reference: the dedupe must
    // read the set, not the object identity.
    forge.arrive({
      kind: 'update',
      update: { tasks_changed: { key: LEAD, tasks: [{ ...task }] } },
    });
    await settle();
    expect(forge.refreshed, 'an unchanged task set asked for another read').toEqual([HOME]);

    forge.arrive(moved);
    await settleFloor();
    expect(forge.refreshed, 'a moved task set did not ask for a read').toEqual([HOME, HOME]);
    stop();
  });

  /** An empty project must compare too: `[]` again is no read, a deletion is. */
  it('skips an unchanged empty set and reads a deletion', async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});
    const empty: ServerMessage = {
      kind: 'update',
      update: { tasks_changed: { key: LEAD, tasks: [] } },
    };
    const one: ServerMessage = {
      kind: 'update',
      update: {
        tasks_changed: {
          key: LEAD,
          tasks: [
            {
              id: 't1',
              project_name: 'proj',
              subject: 'a row',
              status: 'pending',
            } as unknown as Task,
          ],
        },
      },
    };

    forge.arrive(empty);
    await settle();
    expect(forge.refreshed, 'the first empty set asked for a read').toEqual([HOME]);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: {} });
    await settle();

    forge.arrive(empty);
    await settle();
    expect(forge.refreshed, 'an unchanged empty set asked again').toEqual([HOME]);

    forge.arrive(one);
    await settleFloor();
    expect(forge.refreshed, 'a task added did not ask for a read').toEqual([HOME, HOME]);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: {} });
    await settle();

    forge.arrive(empty);
    await settleFloor();
    expect(forge.refreshed, 'a deletion did not ask for a read').toEqual([HOME, HOME, HOME]);
    stop();
  });

  /**
   * **A malformed read is a failed state, not a healthy empty one** (#2540):
   * the store is already `ready` and its bound cleared by the time the page
   * narrows it, so an empty home would stand with nothing saying why.
   */
  it('draws a malformed read as failed rather than an empty home', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));

    forge.stores.get(HOME)?.set({ agents: undefined, projects: [] });
    forge.arrive({ kind: 'snapshot', subject: HOME, data: { agents: undefined, projects: [] } });
    await settle();

    expect(seen.at(-1)?.wire, 'a malformed read drew a wire').toBeNull();
    expect(seen.at(-1)?.failed, 'a malformed read drew no failed words').toContain('agents');
    stop();
  });

  /**
   * **Every collection check runs, and names its field.** A snapshot with
   * three broken collections names all three, so a check dropped from
   * `malformedOf` fails here instead of drawing a home that reads healthy.
   */
  it('names every collection a malformed read broke', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));

    const bad = {
      agents: [],
      projects: [],
      unseen: [],
      fleet: 'not a list',
      accounts: { loading: 'not a list' },
      dictate: { snapshot: { models: 7 } },
    };
    forge.stores.get(HOME)?.set(bad);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: bad });
    await settle();

    const failed = seen.at(-1)?.failed ?? '';
    expect(failed, 'the fleet check did not run').toContain('fleet was not a list');
    expect(failed, 'the accounts check did not run').toContain('accounts.loading was not a list');
    expect(failed, 'the dictate check did not run').toContain(
      'dictate.snapshot.models was not a list',
    );
    stop();
  });

  /**
   * A good read is the page recovering: the failure belongs to the read that
   * broke, so the next one that lands clears it rather than leaving the page
   * on the old words.
   */
  it('clears the failure when a good snapshot lands', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));
    const good = {
      agents: [],
      projects: [],
      unseen: [],
      accounts: { loading: [] },
      dictate: { snapshot: { models: [] } },
    };

    forge.stores.get(HOME)?.set({ agents: undefined, projects: [] });
    forge.arrive({ kind: 'snapshot', subject: HOME, data: { agents: undefined, projects: [] } });
    await settle();
    expect(seen.at(-1)?.failed, 'the malformed read drew no failure').not.toBeNull();

    forge.stores.get(HOME)?.set(good);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: good });
    await settle();
    expect(seen.at(-1)?.failed, 'the good read kept the old failure').toBeNull();
    expect(seen.at(-1)?.wire, 'the good read drew no wire').not.toBeNull();
    stop();
  });

  /**
   * **A store that reports `ready` and holds nothing lost its answer.** The
   * read landed and its snapshot is gone, so standing on the loading state
   * waits for an answer that already came and went.
   */
  it('draws a ready store with no snapshot as failed, not loading', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));
    expect(seen.at(-1)?.failed, 'a loading read claimed a failure').toBeNull();

    forge.stores.get(HOME)?.set(null);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: null });
    await settle();

    expect(seen.at(-1)?.wire, 'a ready empty store drew a wire').toBeNull();
    expect(seen.at(-1)?.failed, 'a ready empty store fell back to waiting').toBe(
      'the snapshot was empty',
    );
    stop();
  });

  /**
   * **A fold never repaints a failed read as healthy.** An empty or skewed
   * answer is its own state, drawn over no record; a later frame that folds
   * into the last good snapshot would erase the failure the state exists to
   * draw (rule 22), so those branches clear the record instead.
   */
  it('does not repaint a failed read with a fold', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe(($next) => {
      seen.push($next);
    });
    const wire = idleHome();
    forge.stores.get(HOME)?.set(wire);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: wire });
    expect(seen.at(-1)?.wire, 'the good read did not land').not.toBeNull();

    forge.stores.get(HOME)?.set(null);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: null });
    expect(seen.at(-1)?.failed, 'the empty answer did not draw a failure').not.toBeNull();

    forge.arrive({
      kind: 'update',
      update: { service_status: { severity: 'error', message: 'the core is down' } },
    });
    await settle();
    expect(seen.at(-1)?.wire, 'a fold repainted the snapshot the failure replaced').toBeNull();
    expect(seen.at(-1)?.failed, 'a fold erased the failure state').not.toBeNull();
    stop();
  });

  /**
   * The core's service line reaches the page's own store, not only a re-read:
   * the snapshot holds no such field, so the report is what a page has to draw
   * a refused edit with - and a report dropped here is a refusal that reads as
   * an edit that landed.
   */
  it("carries the core's service report into the next read", async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));

    forge.arrive({
      kind: 'update',
      update: {
        service_status: { severity: 'warning', message: 'The board refused a move: nope' },
      },
    });
    await settle();
    // The read the update asked for answers with the words it carried.
    forge.arrive({ kind: 'snapshot', subject: HOME, data: null });
    await settle();

    const last = seen.at(-1);
    expect(last?.report, 'the report never reached the store').toEqual({
      severity: 'warning',
      message: 'The board refused a move: nope',
    });
    stop();
  });

  /**
   * The other side of it: a frame the home does not cover is no reason to
   * read. A chat message moves no row, so the page must not ask again for it.
   */
  it('does not read for a frame the home does not cover', async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});

    // Cast: this drives the door rather than the wire's narrowing, and only
    // the variant name is read at it.
    forge.arrive({
      kind: 'update',
      update: { chat_appended: { key: LEAD, msg: { type: 'assistant' } } },
    } as unknown as ServerMessage);
    await settle();
    expect(forge.refreshed, 'a chat message asked for a home read').toEqual([]);
    stop();
  });

  /**
   * **The predicate's headline, end to end.** A plugin run's frame names no
   * seat, so the server sends it to every home subscriber - and no field of
   * the home's snapshot carries it, so it buys no read. A keyless frame whose
   * data the snapshot DOES carry still asks for exactly one.
   */
  it('reads for a keyless frame the snapshot carries, and not for the plugin family', async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});

    forge.arrive({
      kind: 'update',
      update: { plugins_update_run_finished: {} },
    } as unknown as ServerMessage);
    await settle();
    expect(forge.refreshed, 'a plugin frame asked the home for a read').toEqual([]);

    // The read the frame above would have owed is answered before the next
    // one is asked for, so the second assertion is the predicate's and not
    // the in-flight guard's. The positive control is the other keyless
    // family the home carries, not a fleet redraw.
    forge.arrive({ kind: 'snapshot', subject: HOME, data: null });
    forge.arrive({
      kind: 'update',
      update: { service_status: { severity: 'warning', message: 'a notice' } },
    } as unknown as ServerMessage);
    await settle();
    expect(
      forge.refreshed,
      'a keyless frame the snapshot carries did not ask for one read',
    ).toEqual(['home']);
    stop();
  });

  /** The balance the drawn view holds for the fixture account. */
  function balanceOf(read: HomeRead | undefined): unknown {
    const rows = read?.wire?.accounts.usage ?? [];
    const row = rows.find((entry) => entry.display_name === POOL_ACCOUNT);
    return (row?.snapshot as { balance?: unknown } | undefined)?.balance;
  }

  /**
   * **A usage-only move folds.** The pool's facts and the project rows that
   * derive from them are unchanged, so the frame is the whole answer and no
   * read is spent. A frame from an older server carries no payload and is the
   * wake-up it always was.
   */
  it('folds a usage-only pool move, and reads for a frame that carries none', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));
    forge.stores.get(HOME)?.set(POOL_HOME_OLD);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: POOL_HOME_OLD });
    await settle();

    forge.arrive({
      kind: 'update',
      update: { accounts_changed: POOL },
    } as unknown as ServerMessage);
    await settle();
    expect(forge.refreshed, 'a usage-only pool frame asked for a home read').toEqual([]);
    expect(balanceOf(seen.at(-1)), 'the frame did not replace the usage row').toBe(7.25);

    // An older server's unit variant: the wake-up it always was, so it reads.
    forge.arrive({ kind: 'update', update: 'accounts_changed' });
    await settle();
    expect(forge.refreshed, 'a payloadless pool frame did not ask for a read').toEqual(['home']);
    stop();
  });

  /**
   * A hold-down is a countdown, not a fact: a frame whose only change is
   * `retry_after` folds, or every frame during a backoff would buy a read.
   */
  it('folds a frame whose only change is a hold-down countdown', async () => {
    const cooling = {
      ...POOL_FACTS,
      loading: [{ ...POOL_FACTS.loading[0], last_error: 'rate_limited', retry_after: 120 }],
      usage: [poolUsage(7.0)],
    };
    const home = { ...POOL_HOME, accounts: cooling };
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));
    forge.stores.get(HOME)?.set(home);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: home });
    await settle();

    const counted = {
      ...cooling,
      loading: [{ ...cooling.loading[0], retry_after: 60 }],
      usage: [poolUsage(7.25)],
    };
    forge.arrive({
      kind: 'update',
      update: { accounts_changed: counted },
    } as unknown as ServerMessage);
    await settle();
    expect(forge.refreshed, 'a countdown that moved asked for a read').toEqual([]);
    expect(balanceOf(seen.at(-1)), 'the countdown frame did not reach the pool').toBe(7.25);
    stop();
  });

  /**
   * **Every fact a project row, the card or the walk is read from keeps the
   * read.** One case per compared field: a field the comparator stops reading
   * is a row that keeps claiming a bind that no longer holds.
   */
  it('reads when a pool frame moves any fact the project rows derive from', async () => {
    const row = POOL_FACTS.loading[0];
    const cases: [string, (pool: typeof POOL) => unknown][] = [
      ['a row that bailed', (pool) => ({ ...pool, loading: [{ ...row, state: 'bailed' }] })],
      [
        'a probe failure',
        (pool) => ({ ...pool, loading: [{ ...row, last_error: 'rate_limited' }] }),
      ],
      ['a probe landing at the cap', (pool) => ({ ...pool, loading: [{ ...row, at_cap: true }] })],
      ['a rotation cooldown', (pool) => ({ ...pool, loading: [{ ...row, cooling: true }] })],
      ['the settled flag', (pool) => ({ ...pool, all_loaded: false })],
      [
        'a listener that stopped',
        (pool) => ({ ...pool, gateway: { ...pool.gateway, ready: false } }),
      ],
      [
        'a listener on another port',
        (pool) => ({ ...pool, gateway: { ...pool.gateway, port: 9 } }),
      ],
      [
        'a bind failure',
        (pool) => ({ ...pool, gateway: { ...pool.gateway, bind_error: 'in use' } }),
      ],
      ['an account that left the pool', (pool) => ({ ...pool, loading: [] })],
      [
        'an account that changed name',
        (pool) => ({ ...pool, loading: [{ ...row, display_name: 'B' }] }),
      ],
    ];
    for (const [what, moved] of cases) {
      const forge = fakeConnection();
      const stop = watchHome(forge.connection).subscribe(() => {});
      forge.stores.get(HOME)?.set(POOL_HOME_OLD);
      forge.arrive({ kind: 'snapshot', subject: HOME, data: POOL_HOME_OLD });
      await settle();
      forge.arrive({
        kind: 'update',
        update: { accounts_changed: moved(POOL) },
      } as unknown as ServerMessage);
      await settle();
      expect(forge.refreshed, `${what} folded instead of reading`).toEqual(['home']);
      stop();
    }
  });

  /**
   * **Rule 25: the fold and the read are one obligation.** A pool applied
   * from a frame must draw exactly what the same pool read from a snapshot
   * draws - the whole band, the gateway card included, and the rail's chip -
   * or the two paths have quietly become two answers.
   */
  it('draws the same band and rail chip from a folded frame as from a snapshot', async () => {
    // A: one snapshot that already states the pool.
    const a = fakeConnection();
    const seenA: HomeRead[] = [];
    const stopA = watchHome(a.connection).subscribe((read) => seenA.push(read));
    a.stores.get(HOME)?.set(POOL_HOME);
    a.arrive({ kind: 'snapshot', subject: HOME, data: POOL_HOME });
    await settle();

    // B: a snapshot with the same facts and an older usage row, then the frame.
    const b = fakeConnection();
    const seenB: HomeRead[] = [];
    const stopB = watchHome(b.connection).subscribe((read) => seenB.push(read));
    b.stores.get(HOME)?.set(POOL_HOME_OLD);
    b.arrive({ kind: 'snapshot', subject: HOME, data: POOL_HOME_OLD });
    await settle();
    b.arrive({
      kind: 'update',
      update: { accounts_changed: POOL },
    } as unknown as ServerMessage);
    await settle();

    const wireA = seenA.at(-1)?.wire ?? null;
    const wireB = seenB.at(-1)?.wire ?? null;
    if (wireA === null || wireB === null) throw new Error('a read did not publish a wire');

    const band = (wire: typeof wireA) => homeView(wire, '127.0.0.1:8790').band;
    expect(band(wireB), 'the folded pool drew a different band').toEqual(band(wireA));
    // The fixture must resolve the cards the pool feeds, or two empty answers
    // would agree; these pin the narrowing of the gateway and the rows too.
    expect(band(wireA).find((entry) => entry.title === 'accounts')?.value).toBe('1 ready');
    expect(band(wireA).find((entry) => entry.title === 'gateway')?.value).toBe('bound :8787');
    expect(railFooter(wireA, LEAD).account.tone, 'the chip did not resolve the pool row').toBe(
      'ok',
    );
    expect(railFooter(wireB, LEAD), 'the folded pool drew a different rail chip').toEqual(
      railFooter(wireA, LEAD),
    );
    stopA();
    stopB();
  });

  /**
   * A pool that lands while a read is in flight is newer than the answer on
   * its way, so it is applied after that answer rather than before it.
   */
  it('applies a pool that landed mid-read after the snapshot', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));

    // A covered frame asks for a read; nothing has answered it yet.
    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'the first update did not ask for a read').toEqual(['home']);

    forge.arrive({
      kind: 'update',
      update: { accounts_changed: POOL },
    } as unknown as ServerMessage);
    // The answer was encoded before the frame and still says the old pool.
    forge.stores.get(HOME)?.set(POOL_HOME_OLD);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: POOL_HOME_OLD });
    await settle();
    expect(balanceOf(seen.at(-1)), 'the snapshot overwrote the pool the frame had carried').toBe(
      7.25,
    );
    expect(forge.refreshed, 'the mid-read frame asked for another read').toEqual(['home']);
    stop();
  });

  /**
   * A frame that lands before the first snapshot is still the newest pool:
   * nothing has been drawn yet, so the fold waits and rides the first answer.
   */
  it('folds a frame that lands before the first snapshot', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));

    forge.arrive({
      kind: 'update',
      update: { accounts_changed: POOL },
    } as unknown as ServerMessage);
    await settle();
    expect(forge.refreshed, 'a pre-snapshot frame asked for a read').toEqual([]);

    forge.stores.get(HOME)?.set(POOL_HOME_OLD);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: POOL_HOME_OLD });
    await settle();
    expect(balanceOf(seen.at(-1)), 'the first snapshot overwrote the earlier frame').toBe(7.25);
    expect(forge.refreshed, 'the first snapshot asked for a read of its own').toEqual([]);
    stop();
  });

  /**
   * **A foreign error settles no read.** A dispatch refusal arriving while a
   * home read is in flight used to free the read (and redraw the held
   * snapshot); that both reverted a fold and let the fold ride over the
   * answer it should have been replaced by. Here the error changes nothing,
   * the fold belongs to the read in flight, and the answer keeps it.
   */
  it('keeps a folded pool across a foreign error that settles no read', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));
    forge.stores.get(HOME)?.set(POOL_HOME_OLD);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: POOL_HOME_OLD });
    await settle();

    // A covered frame asks for a read; the request is in flight.
    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'the first update did not ask for a read').toEqual(['home']);

    forge.arrive({
      kind: 'error',
      what: 'dispatch',
      why: 'that command was refused',
    } as unknown as ServerMessage);
    await settle();
    forge.arrive({
      kind: 'update',
      update: { accounts_changed: POOL },
    } as unknown as ServerMessage);
    await settle();

    // The answer was encoded before the fold and lands after it.
    forge.stores.get(HOME)?.set(POOL_HOME_OLD);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: POOL_HOME_OLD });
    await settle();
    expect(
      balanceOf(seen.at(-1)),
      'a foreign error freed the fold to be overwritten by the answer',
    ).toBe(7.25);
    expect(forge.refreshed, 'a foreign error asked for another read').toEqual(['home']);
    stop();
  });

  /**
   * A fold made during a read does not survive the drop: the reconnect's own
   * snapshot is newer and replaces it.
   */
  it('drops a fold made mid-read when the connection reconnects', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));
    forge.stores.get(HOME)?.set(POOL_HOME_OLD);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: POOL_HOME_OLD });
    await settle();

    forge.arrive(UPDATE);
    await settle();
    forge.arrive({
      kind: 'update',
      update: { accounts_changed: POOL },
    } as unknown as ServerMessage);
    await settle();

    forge.status('closed');
    const reconnected = { ...POOL_HOME, accounts: { ...POOL_FACTS, usage: [poolUsage(6.5)] } };
    forge.stores.get(HOME)?.set(reconnected);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: reconnected });
    await settle();
    expect(
      balanceOf(seen.at(-1)),
      'a fold from before the drop rode over the reconnect snapshot',
    ).toBe(6.5);
    stop();
  });

  /**
   * The reconnect's own answer is a fresh snapshot and replaces a fold made
   * before the drop.
   */
  it('lets the reconnect snapshot replace a fold made before the drop', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));
    forge.stores.get(HOME)?.set(POOL_HOME_OLD);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: POOL_HOME_OLD });
    await settle();
    forge.arrive({
      kind: 'update',
      update: { accounts_changed: POOL },
    } as unknown as ServerMessage);
    await settle();

    forge.status('closed');
    const reconnected = { ...POOL_HOME, accounts: { ...POOL_FACTS, usage: [poolUsage(6.5)] } };
    forge.stores.get(HOME)?.set(reconnected);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: reconnected });
    await settle();
    expect(balanceOf(seen.at(-1)), 'the reconnect snapshot did not replace the fold').toBe(6.5);
    stop();
  });

  /**
   * A payload this client cannot read is not the pool: an older or newer
   * server shape falls back to the read rather than folding an empty pool over
   * a live one.
   */
  it('reads for a pool frame whose payload is not the pool shape', async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});
    forge.stores.get(HOME)?.set(POOL_HOME_OLD);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: POOL_HOME_OLD });
    await settle();

    forge.arrive({
      kind: 'update',
      update: { accounts_changed: { accounts: POOL } },
    } as unknown as ServerMessage);
    await settle();
    expect(forge.refreshed, 'an unreadable pool payload folded instead of reading').toEqual([
      'home',
    ]);
    stop();
  });

  /**
   * A read is a full encode on the server, so a subscription nobody draws from
   * must not be left asking for one. The last subscriber leaving is what
   * releases the subscription, the listeners and any read already queued: a
   * replaced connection otherwise stays counted against a seat it is not
   * showing, re-reading the fleet for a page that is gone.
   */
  it('stops asking for the home once the last subscriber goes', async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});

    // While it is watched: an update is answered with one read, and the answer
    // to that read is what lets the next one be asked for.
    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'a watched home did not answer an update with a read').toEqual([
      'home',
    ]);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: null });

    // The release, with a read already queued behind the last subscriber.
    forge.arrive(UPDATE);
    stop();
    expect(forge.unsubscribed, 'the last subscriber left the home subscribed').toEqual(['home']);

    // The queued read does not go, and no frame after it can ask for one.
    await settle();
    expect(forge.refreshed, 'a read queued behind the last subscriber still went').toHaveLength(1);
    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'a frame still reached a released subscription').toHaveLength(1);

    // And nothing is left attached to the connection, the status listener
    // included.
    expect(forge.listening(), 'the release left a listener on the connection').toBe(0);
  });
});
