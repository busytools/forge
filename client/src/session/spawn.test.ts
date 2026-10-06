// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { writable } from 'svelte/store';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import type { Connection } from '../socket';
import type { Store, StoreValue } from '../stores';
import type { HomeWire } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import { closeSeat, forgetClosed } from './close';
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
    frame: () => false,
    onBrowserAsk: () => () => {},
    browserRole: () => false,
    onBrowserRole: () => () => {},
    takeBrowserRole: () => {},
    onMessage: () => () => {},
    onStatus: () => () => {},
    store: () => undefined,
    settings: () => null,
    skew: () => null,
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

/** The fixture's home with its lead row in the lifecycle named. */
function homeAs(lifecycle: HomeWire['agents'][number]['lifecycle']): HomeWire {
  const template = homeWire.agents[0];
  if (template === undefined) throw new Error('the fixture holds no agent');
  return { ...homeWire, agents: [{ ...template, lifecycle }] };
}

let app: Record<string, unknown> | null = null;

beforeEach(() => {
  // The close marks are module state; an empty roster forgets all of them.
  forgetClosed({ ...homeWire, agents: [] });
});

afterEach(() => {
  if (app !== null) void unmount(app);
  app = null;
  document.body.innerHTML = '';
});

function openWith(wire: HomeWire, slot: SessionSlot = LEAD) {
  const { connection, sent } = recording();
  app = mount(SpawnHarness, {
    target: document.body,
    props: { slot, connection, home: wire },
  });
  flushSync();
  return { sent, page: () => (app as unknown as { page: { home: HomeWire } }).page };
}

function open(withLead: boolean, slot: SessionSlot = LEAD) {
  return openWith(home(withLead), slot);
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

  /**
   * A named lead the roster has landed asleep is the same wake (#1704's rule:
   * opening the page starts it), and the page's own words promise the spawn -
   * so the ask goes rather than the words standing over nothing.
   */
  it('asks the core to start a lead the roster names asleep', () => {
    const { sent } = openWith(homeAs('Sleeping'));

    expect(sent, 'a sleeping lead was opened and nothing asked').toEqual([
      {
        command: {
          spawn_project: { project_name: 'proj', launch_settings: {} },
        },
      },
    ]);
  });

  it('asks the core to start a lead the roster names logged out', () => {
    const { sent } = openWith(homeAs('LoggedOut'));

    expect(sent, 'a logged-out lead was opened and nothing asked').toHaveLength(1);
  });

  /**
   * The latch is cleared the moment the seat is up, so a seat that comes up
   * and goes away again in the same mount is a FRESH wake: holding the latch
   * past the landing would leave the second wake with a promise nothing
   * keeps.
   */
  it('asks again when a seat that landed goes away again in the same mount', () => {
    const { sent, page } = open(false);

    page().home = home(true);
    flushSync();
    page().home = home(false);
    flushSync();

    expect(sent, 'the second wake was never asked for').toHaveLength(2);
  });

  /**
   * The promise has a state it must not make: a seat this client just closed
   * carries the mark because the click was made here, and a roster landing it
   * asleep is that close arriving - not a wake for this page to start again.
   */
  it('asks for nothing at a seat this client just closed', () => {
    const wire = homeAs('Sleeping');
    const moved = closeSeat(
      { dispatch: () => null } as unknown as Connection,
      wire,
      LEAD,
      { ...LEAD, label: 'w1' },
      0,
    );
    expect(moved, 'the close never went').toBe(true);

    const { sent } = openWith(wire);

    expect(sent, 'a close read as a wake and started the seat again').toEqual([]);
  });
});
