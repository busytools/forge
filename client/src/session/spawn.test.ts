// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { writable } from 'svelte/store';
import { afterEach, describe, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import type { Connection } from '../socket';
import type { Store, StoreValue } from '../stores';
import type { HomeWire } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import SpawnHarness from './SpawnHarness.svelte';

const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

/**
 * A store for one subscription, still waiting for its answer.
 *
 * The seat subscribes as part of drawing, so the subscription is answered with
 * an empty store rather than refused: what this file is about is the command
 * the page sends, and refusing the read would fail every case for a reason
 * that has nothing to do with it.
 */
function held(): Store {
  const value = writable<StoreValue>({
    snapshot: null,
    updates: [],
    state: { kind: 'loading' },
    dropped: 0,
  });
  return {
    subject: { session: LEAD },
    value,
    snapshot: () => null,
    updates: () => [],
    state: () => ({ kind: 'loading' }),
    dropped: () => 0,
    set: () => {},
    push: () => {},
    refuse: () => {},
  };
}

/** A connection that records what the page asked it to do. */
function recording(): { connection: Connection; sent: Record<string, unknown>[] } {
  const sent: Record<string, unknown>[] = [];
  const connection: Connection = {
    subscribe: () => held(),
    unsubscribe: () => {},
    refresh: () => {},
    dispatch: (command) => {
      sent.push({ command });
      return null;
    },
    more: () => false,
    devices: () => false,
    onMessage: () => () => {},
    onStatus: () => () => {},
    store: () => undefined,
    settings: () => null,
    status: () => 'open',
    close: () => {},
  };
  return { connection, sent };
}

/** The fixture's home with the lead row kept or dropped, which is a seat that
 * is up or one nothing is running. */
function home(lead: boolean): HomeWire {
  return { ...homeWire, agents: lead ? homeWire.agents : [] };
}

let app: Record<string, unknown> | null = null;

afterEach(() => {
  if (app !== null) void unmount(app);
  app = null;
  document.body.innerHTML = '';
});

function open(withLead: boolean, slot: SessionSlot = LEAD) {
  const { connection, sent } = recording();
  app = mount(SpawnHarness, {
    target: document.body,
    props: { slot, connection, home: home(withLead) },
  });
  flushSync();
  return { sent, page: () => (app as unknown as { page: { home: HomeWire } }).page };
}

describe("starting a project's lead from its own page", () => {
  /**
   * The rail's click is a link to the seat, and a seat nothing is running
   * behind used to draw "not running" and start nothing. The terminal's own
   * click spawns the project; so does landing here.
   */
  it('asks the core to start a lead nothing is running behind', () => {
    const { sent } = open(false);

    expect(sent, 'one ask, for the project the seat names').toEqual([
      {
        command: {
          spawn_project: { project_name: 'proj', launch_settings: {} },
        },
      },
    ]);
  });

  it('asks for nothing when the seat is already up', () => {
    const { sent } = open(true);

    expect(sent, 'a seat the roster names is a project already started').toEqual([]);
  });

  /**
   * A project's lead is the one seat the core can start by name: a worker is
   * spawned by the lead that owns it, so a worker seat with nothing behind it
   * asks for nothing rather than for a spawn the core cannot place.
   */
  it('asks for nothing when the seat is a worker', () => {
    const { sent } = open(false, { ...LEAD, label: 'implementer' });

    expect(sent, 'only a lead is a spawn the core can place').toEqual([]);
  });

  /**
   * The ask is an effect, so it re-runs on every frame that touches the
   * roster - and the roster names the seat only once `Spawning` has landed.
   * Without the latch the page would ask again on each of those frames, which
   * the terminal refuses a second click for.
   */
  it('asks once, however many frames pass before the seat lands', () => {
    const { sent, page } = open(false);

    for (let frame = 0; frame < 3; frame += 1) {
      page().home = home(false);
      flushSync();
    }
    page().home = home(true);
    flushSync();

    expect(sent, 'one ask for the whole approach, not one per frame').toHaveLength(1);
    const drawn = document.body.textContent ?? '';
    expect(drawn, 'and the seat stops drawing as one nothing runs').not.toContain('not running');
  });
});
