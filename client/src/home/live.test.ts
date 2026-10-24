import { describe, expect, it } from 'vitest';

import type { ServerMessage, Subject } from '../protocol';
import { PROTOCOL_VERSION } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import { Stores } from '../stores';
import type { Task } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import { railFooter } from '../session/view';
import { watchHome, type HomeRead } from './live';
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
      chip: { account_name: POOL_ACCOUNT, state: 'ready' },
    },
  ],
  unseen: [],
  accounts: POOL,
  dictate: { snapshot: { models: [] } },
};

/** The same home with the older usage row; its facts match `POOL`. */
const POOL_HOME_OLD = { ...POOL_HOME, accounts: POOL_OLD };

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
    refresh(what) {
      refreshed.push(what);
      return true;
    },
    browserView: () => false,
    browserControl: () => false,
    browserNav: () => false,
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
    dispatch: () => null,
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

/** Give a timer a chance to fire, for everything it would have set off to show. */
async function settle(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 100));
}

describe('the home over a connection', () => {
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

    // Its answer lands, and the update that arrived meanwhile is asked for.
    forge.arrive({ kind: 'snapshot', subject: HOME, data: null });
    await settle();
    expect(forge.refreshed, 'the update that landed mid-read was dropped').toEqual([
      'home',
      'home',
    ]);
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
   * **The turn's start is a read, and #1887 is what a page that skipped it
   * drew.** A prompt's lifecycle is the earliest frame that says a turn was
   * accepted: the roster's `running` comes from `turn_pending`, stamped when
   * the prompt is routed, so a read asked at `queued` or `started` is
   * answered with the turn already on. Without it the row kept the idle it
   * last read until some unrelated frame happened to be news, measured at
   * ~40s while an agent ran underneath.
   */
  it("reads when a prompt's life moves", async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});

    forge.arrive({
      kind: 'update',
      update: { prompt_lifecycle: { key: LEAD, uuid: 'p1', state: 'started' } },
    });
    await settle();
    expect(forge.refreshed, "a turn's start did not ask for exactly one read").toEqual(['home']);
    stop();
  });

  /**
   * The background registry moves a row too - `has_background_work` is the
   * promotion that draws an idle session as running - and its read is asked
   * for through the conversation frame the same CLI event arrives in. The
   * registry's own mirror is the inspector's copy and stays out of the fleet
   * table; this is the arm that keeps a backgrounded seat from reading as
   * idle.
   */
  it('reads when the CLI announces background work', async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});

    forge.arrive({
      kind: 'update',
      update: {
        chat_appended: {
          key: LEAD,
          msg: { type: 'system', subtype: 'background_tasks_changed', tasks: [] },
        },
      },
    } as unknown as ServerMessage);
    await settle();
    expect(forge.refreshed, 'a background task starting asked for no read').toEqual([HOME]);
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
    await settle();
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
    await settle();
    expect(forge.refreshed, 'a task added did not ask for a read').toEqual([HOME, HOME]);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: {} });
    await settle();

    forge.arrive(empty);
    await settle();
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
