import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { ServerMessage, Subject } from '../protocol';
import { PROTOCOL_VERSION } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import { Stores } from '../stores';
import type { Task } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import { railFooter } from '../session/view';
import { homeWire } from '../dev/fixture.data';
import { watchHome, type HomeRead } from './live';
import { homeView } from './view';

const HOME: Subject = 'home';
const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

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

/** The same facts with an older usage row, as the record before the frame. */
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

/** One prompt-lifecycle frame for the fixture's seat. */
function life(state: string, uuid = 'p1'): ServerMessage {
  return { kind: 'update', update: { prompt_lifecycle: { key: LEAD, uuid, state } } };
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
 * to hand it a frame. The stores are the real ones, because a subscription's
 * lifecycle is what this fakes around rather than anything about a store.
 */
function fakeConnection() {
  const stores = new Stores();
  const subscribed: Subject[] = [];
  const options: { answering?: boolean }[] = [];
  const unsubscribed: Subject[] = [];
  const refreshed: Subject[] = [];
  const messages = new Set<(message: ServerMessage) => void>();
  const statuses = new Set<(status: ConnectionStatus) => void>();
  let status: ConnectionStatus = 'open';

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
    status: () => status,
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
    setStatus(next: ConnectionStatus) {
      status = next;
      for (const fn of [...statuses]) fn(next);
    },
  };
}

/** Deliver a whole read through the store, as the socket does. */
function answer(forge: ReturnType<typeof fakeConnection>, data: unknown, version = 1): void {
  forge.stores.get(HOME)?.set(data, version);
}

/** Deliver a patch through the store, as the socket does after applying it. */
function patched(forge: ReturnType<typeof fakeConnection>, data: unknown, version = 2): void {
  forge.stores.get(HOME)?.applied(version, data);
}

/** Let the page's own notifications settle; a store write redraws it at once. */
async function settle(): Promise<void> {
  await Promise.resolve();
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe('the home over a connection', () => {
  /**
   * **The connection declares itself answerable, usually on its first
   * subscribe.** The role belongs to the connection and only ever rises, and
   * this subscribe is usually the first the app makes.
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
   * **The page draws the store, and a patch redraws it.** A whole read seeds
   * the record; a patch the socket applied in place moves it - with no ask
   * for a home read anywhere in the path.
   */
  it('draws the record and every patch the socket applied, reading nothing', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));

    answer(forge, POOL_HOME_OLD, 1);
    expect(seen.at(-1)?.wire?.accounts, 'the whole read did not draw').toBeDefined();

    // The socket writes the patch's fields over the record before the page
    // sees it; this is that merged record.
    patched(forge, { ...POOL_HOME_OLD, accounts: POOL }, 2);
    await settle();
    const rows = seen.at(-1)?.wire?.accounts.usage ?? [];
    expect(
      rows.find((row) => row.display_name === POOL_ACCOUNT)?.snapshot,
      'the patch did not move the pool',
    ).toMatchObject({ balance: 7.25 });
    expect(forge.refreshed, 'a read was asked for').toEqual([]);
    stop();
  });

  /** **An update that lands mid-record is not dropped (#1885).** The task
   * frame folds into the drawn fleet row at once; the patch follows. */
  it('counts a task frame into the fleet row without a read', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe(($next) => {
      seen.push($next);
    });
    answer(forge, homeWire);

    forge.arrive({
      kind: 'update',
      update: {
        tasks_changed: {
          key: LEAD,
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

  /** **A prompt's life folds into the drawn row**, counted by uuid, so a
   * queued prompt behind a running one is a second entry. */
  it("folds a prompt's life into the drawn row without a read", async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));
    answer(forge, idleHome());

    forge.arrive(life('queued', 'p1'));
    forge.arrive(life('queued', 'p1'));
    expect(forge.refreshed, 'a duplicate uuid asked for a read').toEqual([]);
    forge.arrive(life('queued', 'p2'));
    await settle();
    const agents = seen.at(-1)?.wire?.agents ?? [];
    expect(agents[0]?.lifecycle, 'two queued prompts did not draw the row running').toBe('Running');

    forge.arrive(life('completed', 'p1'));
    forge.arrive(life('completed', 'p2'));
    await settle();
    expect(forge.refreshed, 'the empty set asked for a read').toEqual([]);
    stop();
  });

  /** **A record replacement re-bases the prompt set**, and a drop clears it,
   * so a reconnect's first answer starts from the snapshot alone. */
  it('keeps a second queued prompt across a record, and forgets it on a reconnect', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));
    answer(forge, idleHome(), 1);
    forge.arrive(life('queued', 'p1'));
    forge.arrive(life('queued', 'p2'));

    // A newer record says the seat is running; the uuids this page saw stay.
    answer(forge, { ...idleHome(), agents: [{ ...IDLE_AGENT, lifecycle: 'Running' }] }, 2);
    forge.arrive(life('completed', 'p1'));
    await settle();
    const afterOne = seen.at(-1)?.wire?.agents[0]?.lifecycle;
    expect(afterOne, 'one terminal emptied a set that still held another prompt').toBe('Running');

    forge.setStatus('connecting');
    answer(forge, { ...idleHome(), agents: [{ ...IDLE_AGENT, lifecycle: 'Running' }] }, 3);
    forge.arrive(life('completed', 'p2'));
    await settle();
    expect(forge.refreshed, 'a fold asked for a read').toEqual([]);
    stop();
  });

  /** The CLI's background registry folds into the drawn row. */
  it('folds the CLI background registry into the drawn row without a read', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));
    answer(forge, idleHome());
    forge.arrive(background([{ id: 'b1', command: 'sleep 30' }]));
    await settle();
    expect(seen.at(-1)?.wire?.agents[0]?.has_background_work).toBe(true);
    expect(forge.refreshed).toEqual([]);
    stop();
  });

  /** The service report, the fatal and the vault lock are fields the frames
   * carry whole, so the band and the settings card redraw without a read. */
  it("folds the core's service report, a fatal, a vault lock and the enrolments", async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));
    answer(forge, { ...idleHome(), vault: { locked: true, idle_minutes: 5 } });

    forge.arrive({
      kind: 'update',
      update: { service_status: { severity: 'error', message: 'the core is down' } },
    });
    await settle();
    expect(seen.at(-1)?.report?.message).toBe('the core is down');

    forge.arrive({
      kind: 'update',
      update: { fatal_error: { message: 'it stopped' } },
    } as unknown as ServerMessage);
    await settle();
    expect(seen.at(-1)?.wire?.fatal_error).toBe('it stopped');

    forge.arrive({ kind: 'update', update: { vault_lock_changed: { locked: false } } });
    await settle();
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
    expect(seen.at(-1)?.wire?.biometric.value).toHaveLength(1);
    expect(forge.refreshed, 'a fold asked for a whole read').toEqual([]);
    stop();
  });

  /** **A re-announced task set is no redraw** (#2540); only a moved set is. */
  it('skips a task set that has not moved', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));
    answer(forge, idleHome());
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
    const first = seen.at(-1)?.wire?.fleet.find((entry) => entry.project === 'proj')?.queue;
    forge.arrive({ ...same, update: { tasks_changed: { key: LEAD, tasks: [{ ...task }] } } });
    await settle();
    const second = seen.at(-1)?.wire?.fleet.find((entry) => entry.project === 'proj')?.queue;
    expect(second, 'an unchanged set moved the row').toBe(first);

    forge.arrive(moved);
    await settle();
    const third = seen.at(-1)?.wire?.fleet.find((entry) => entry.project === 'proj')?.queue;
    expect(third, 'a moved set did not move the row').toBe(0);
    expect(forge.refreshed).toEqual([]);
    stop();
  });

  /** **A pool frame folds, and draws exactly what the same pool read from a
   * record draws** - rule 25's read/live parity. */
  it('draws the same band and rail chip from a folded frame as from a record', async () => {
    const a = fakeConnection();
    const seenA: HomeRead[] = [];
    const stopA = watchHome(a.connection).subscribe((read) => seenA.push(read));
    answer(a, POOL_HOME);
    await settle();

    const b = fakeConnection();
    const seenB: HomeRead[] = [];
    const stopB = watchHome(b.connection).subscribe((read) => seenB.push(read));
    answer(b, POOL_HOME_OLD);
    await settle();
    b.arrive({ kind: 'update', update: { accounts_changed: POOL } } as unknown as ServerMessage);
    await settle();

    const wireA = seenA.at(-1)?.wire ?? null;
    const wireB = seenB.at(-1)?.wire ?? null;
    if (wireA === null || wireB === null) throw new Error('a read did not publish a wire');
    const band = (wire: typeof wireA) => homeView(wire, '127.0.0.1:8790').band;
    expect(band(wireB), 'the folded pool drew a different band').toEqual(band(wireA));
    expect(band(wireA).find((entry) => entry.title === 'accounts')?.value).toBe('1 ready');
    expect(railFooter(wireA, LEAD).account.tone, 'the chip did not resolve the pool row').toBe(
      'ok',
    );
    stopA();
    stopB();
  });

  /** **A frame no fold can place is left to the patch.** Nothing asks for a
   * read: the server marked the home dirty and its patch redraws the store. */
  it('leaves a frame it cannot fold to the patch, asking nothing', async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});
    answer(forge, idleHome());
    forge.arrive({
      kind: 'update',
      update: { task_verdict: { key: LEAD } },
    } as unknown as ServerMessage);
    await settle();
    expect(forge.refreshed, 'an unfoldable frame bought a read').toEqual([]);
    stop();
  });

  it('draws a malformed read as failed rather than an empty home', () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));
    answer(forge, { projects: 'not a list' });
    const failure = seen.at(-1)?.failed ?? '';
    expect(failure).toContain('projects');
    expect(seen.at(-1)?.wire, 'a malformed read drew a wire').toBeNull();
    stop();
  });

  it('clears the failure when a good record lands', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));
    answer(forge, { projects: 'not a list' }, 1);
    expect(seen.at(-1)?.failed).not.toBeNull();
    answer(forge, idleHome(), 2);
    await settle();
    expect(seen.at(-1)?.failed, 'the failure outlived the good record').toBeNull();
    expect(seen.at(-1)?.wire, 'the good record did not draw').not.toBeNull();
    stop();
  });

  it('draws a ready store with no snapshot as failed, not loading', () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));
    forge.stores.get(HOME)?.set(null, 1);
    expect(seen.at(-1)?.failed).toBe('the snapshot was empty');
    stop();
  });

  /**
   * **A patch the page could not place is drawn, not dropped.** Rule 25: the
   * socket holds it in the store and the page says so with a count.
   */
  it('draws a held patch the page could not place', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe((read) => seen.push(read));
    answer(forge, idleHome());

    forge.stores.get(HOME)?.unplaced({ a_field: 'unreadable' });
    await settle();
    expect(seen.at(-1)?.heldPatches, 'the held patch was not drawn').toHaveLength(1);
    expect(forge.refreshed, 'a held patch asked for a read').toEqual([]);
    stop();
  });

  /** **A fold never repaints a failed read as healthy** (rule 22). */
  it('does not repaint a failed read with a fold', async () => {
    const forge = fakeConnection();
    const seen: HomeRead[] = [];
    const stop = watchHome(forge.connection).subscribe(($next) => {
      seen.push($next);
    });
    answer(forge, idleHome(), 1);
    expect(seen.at(-1)?.wire, 'the good read did not land').not.toBeNull();

    answer(forge, null, 2);
    expect(seen.at(-1)?.failed, 'the empty answer did not draw a failure').not.toBeNull();

    forge.arrive({
      kind: 'update',
      update: { service_status: { severity: 'error', message: 'the core is down' } },
    });
    await settle();
    expect(seen.at(-1)?.wire, 'a fold repainted the record the failure replaced').toBeNull();
    expect(seen.at(-1)?.failed, 'a fold erased the failure state').not.toBeNull();
    stop();
  });

  /** **No beat, no timer, no read for a frame.** A watched home whose record
   * does not move is silent for half an hour. */
  it('sends nothing and arms nothing across half an hour of an unchanged home', async () => {
    const forge = fakeConnection();
    let draws = 0;
    const stop = watchHome(forge.connection).subscribe(() => {
      draws += 1;
    });
    answer(forge, idleHome());
    const drawn = draws;
    await vi.advanceTimersByTimeAsync(30 * 60_000);
    expect(forge.refreshed, 'the home re-read itself').toEqual([]);
    expect(draws, 'the home redrew without news').toBe(drawn);
    expect(vi.getTimerCount(), 'a timer was left armed behind the page').toBe(0);
    stop();
  });

  it('stops asking for the home once the last subscriber goes', () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});
    stop();
    expect(forge.unsubscribed).toEqual([HOME]);
    expect(forge.listening(), 'listeners outlived the last reader').toBe(0);
  });
});
