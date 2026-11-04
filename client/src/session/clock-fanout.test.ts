// @vitest-environment jsdom
/**
 * The seat's 30 s clock tick must re-render the age text and nothing else.
 *
 * **What the tick re-ran.** The page handed `now` down as a plain number, so
 * every `$derived` reading it was rebuilt: `railGroups`, the rail footer and
 * the inspector's schedule, monitor and review builders. The conversation
 * column never read that clock; this case pins that it stays out too.
 *
 * The fix hands the age leaf a store instead, so this case counts renders: the
 * conversation and rail builders must not run again when the clock moves,
 * while the age cell must. Mutation-verified by putting the plain number back:
 * the tick then calls `railGroups` and this case fails.
 */
import { flushSync, mount, unmount } from 'svelte';
import { writable } from 'svelte/store';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { installResizeObserver } from '../chat/testing/viewport';
import { boardWire, homeWire } from '../dev/fixture.data';
import load from '../dev/fixtures/session-load.json';
import session from '../dev/fixtures/session.json';
import { PROTOCOL_VERSION } from '../protocol';
import type { ServerMessage, Subject } from '../protocol';
import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';
import type { HomeWire } from '../wire/home';

installResizeObserver();

// The conversation draws every turn it is handed, rather than the virtualised
// window: what this case counts is the builders, and that needs the rows drawn.
vi.mock('virtua/svelte', async () => {
  const { default: List } = await import('../chat/testing/List.svelte');
  return { VList: List };
});

vi.mock('../session/view', async (importOriginal) => {
  const real = await importOriginal<typeof import('./view')>();
  const { counting } = await import('./testing/counts');
  return {
    ...real,
    railGroups: counting('railGroups', real.railGroups),
    railFooter: counting('railFooter', real.railFooter),
    seatScheduleRows: counting('seatScheduleRows', real.seatScheduleRows),
    monitorRows: counting('monitorRows', real.monitorRows),
    reviewRows: counting('reviewRows', real.reviewRows),
    reviewSets: counting('reviewSets', real.reviewSets),
  };
});

vi.mock('../chat/units', async (importOriginal) => {
  const real = await importOriginal<typeof import('../chat/units')>();
  const { counting } = await import('../session/testing/counts');
  return { ...real, turnFold: counting('turnFold', real.turnFold) };
});

const { default: Harness } = await import('./testing/ClockHarness.svelte');
const counts = await import('./testing/counts');

const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };
const SUBJECT: Subject = { session: LEAD };

/** A start instant the fixture's stamp is measured against, so the age cell
 *  reads "now" at mount and "1m" after one tick. */
const START = 1_800_000_000_000;
const STAMP = Math.floor(START / 1000) - 59;

function asleepHome(): HomeWire {
  const home = structuredClone(homeWire);
  const lead = home.agents[0];
  if (lead === undefined) throw new Error('the fixture holds no lead');
  lead.lifecycle = 'Sleeping';
  lead.pending = null;
  lead.pending_depth = 0;
  lead.last_activity = { secs_since_epoch: STAMP, nanos_since_epoch: 0 };
  const project = home.projects[0];
  if (project === undefined) throw new Error('the fixture holds no project');
  project.project.sessions = [];
  return home;
}

function seats(turns: unknown[]): unknown {
  // `cursor` present, even null, so the snapshot seeds its page rather than
  // being read as an older server's record that has to be asked for again.
  return { ...session, conversation: { turns, compaction_count: 0, cursor: null }, ...load };
}

function seat(turns: unknown[] = load.turns) {
  const listeners = new Set<(message: ServerMessage) => void>();
  const held: unknown = seats(turns);
  const store = {
    subject: SUBJECT,
    value: writable({ snapshot: held, updates: [], state: { kind: 'ready' }, dropped: 0 }),
    snapshot: () => held,
    updates: () => [],
    state: () => ({ kind: 'ready' }) as const,
    dropped: () => 0,
    set: () => undefined,
    push: () => undefined,
    refuse: () => undefined,
  };
  const emit = (message: ServerMessage): void => {
    for (const listener of listeners) listener(message);
  };
  const connection = {
    subscribe: () => store,
    unsubscribe: () => undefined,
    refresh: () => undefined,
    dispatch: () => null,
    more: () => false,
    onMessage: (fn: (message: ServerMessage) => void) => {
      listeners.add(fn);
      emit({ kind: 'snapshot', subject: SUBJECT, data: held });
      return () => listeners.delete(fn);
    },
    onStatus: () => () => undefined,
    store: () => undefined,
    settings: () => null,
    serverProtocol: () => PROTOCOL_VERSION,
    status: () => 'open' as const,
    close: () => undefined,
  };
  return connection as unknown as Connection;
}

let app: Record<string, unknown> | null = null;

beforeEach(() => {
  counts.clear();
  vi.useFakeTimers();

  vi.setSystemTime(START);
});

afterEach(async () => {
  vi.useRealTimers();
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

describe('the seat clock tick', () => {
  it('re-renders the age text without re-running the conversation or rail builders', () => {
    app = mount(Harness, {
      target: document.body,
      props: {
        slot: LEAD,
        connection: seat(),
        wire: asleepHome(),
        board: { wire: boardWire, refused: null },
      },
    });
    flushSync();

    const drawn = counts.tally().find((entry) => entry.name === 'turnFold');
    expect(drawn, 'the mount drew no conversation, so a quiet tick proves nothing').toBeDefined();
    counts.clear();

    const age = document.querySelector('.age');
    expect(age, 'the fixture draws no age cell to read').not.toBeNull();
    expect(age?.textContent, 'the cell reads the stamp as "now" at mount').toBe('now');

    vi.advanceTimersByTime(30_000);
    flushSync();

    expect(
      counts.tally().find((entry) => entry.name === 'turnFold'),
      'the tick re-ran the conversation row builder',
    ).toBeUndefined();
    expect(
      counts.tally().find((entry) => entry.name === 'railGroups'),
      'the tick re-ran the rail builder',
    ).toBeUndefined();
    expect(
      document.querySelector('.age')?.textContent,
      'the age cell did not follow the clock',
    ).toBe('1m');
  });
});
