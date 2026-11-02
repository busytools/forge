// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import type { Command } from '../protocol';
import type { Connection } from '../socket';
import type { AgentRow, AttentionRow, HomeWire } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import Rail from './Rail.svelte';

/**
 * The rail's controls, pressed in a real DOM: what each press actually hands
 * the connection.
 *
 * The failed project row is the case here because it is the one a reader
 * closes to dismiss a spawn that never came up. Catches a chip hidden from a
 * failed row, or one wired to a command other than the close the row's seat
 * takes.
 */
const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };
const WORKER: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'w1' };
const ELSEWHERE: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'other' };

let app: Record<string, unknown> | null = null;
let acted: Command[] = [];

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

beforeEach(() => {
  acted = [];
});

/** A connection that records what a press dispatches and refuses every read. */
function recording(): Connection {
  const refuse = (): never => {
    throw new Error('the press reached the connection beyond dispatch');
  };
  return {
    subscribe: refuse,
    unsubscribe: refuse,
    refresh: refuse,
    showing: refuse,
    relist: refuse,
    declare: refuse,
    release: refuse,
    dispatch: (command: Command) => {
      acted.push(command);
      return Promise.resolve(null);
    },
    more: refuse,
    commitDiff: refuse,
    fileContent: refuse,
    readDocument: refuse,
    seatRange: refuse,
    frame: refuse,
    browserInput: refuse,
    onBrowserFrame: refuse,
    onMessage: refuse,
    onStatus: refuse,
    store: refuse,
    settings: refuse,
    skew: refuse,
    serverProtocol: () => 0,
    status: refuse,
    probe: refuse,
    close: refuse,
  };
}

/** The fixture's lead as a spawn that never came up. */
function failedLead(): HomeWire {
  const template = homeWire.agents[0];
  if (template === undefined) throw new Error('the fixture holds no agent');
  const lead: AgentRow = {
    ...template,
    slot: LEAD,
    lifecycle: 'Failed',
    reason: 'the account was rate limited',
    died: false,
    failed_turn: null,
    pending: null,
  };
  return { ...homeWire, agents: [lead] };
}

describe('the rail on a failed project row', () => {
  it('sends the lead close when its chip is pressed', () => {
    app = mount(Rail, {
      target: document.body,
      props: {
        home: failedLead(),
        current: ELSEWHERE,
        now: 0,
        connection: recording(),
      },
    });
    flushSync();

    const chip = document.querySelector('button[aria-label="close proj"]');
    expect(chip, 'the failed row draws no close chip').not.toBeNull();
    chip?.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    flushSync();

    expect(acted, 'the press sent something other than the lead close').toEqual([
      { close_session: { session_key: LEAD } },
    ]);
  });
});

/** A wait at a second, as the wire carries it. */
const at = (secs: number) => ({ secs_since_epoch: secs, nanos_since_epoch: 0 });

/** The fixture's lead and a worker, neither holding a prompt, over three waits. */
function waiting(waits: AttentionRow[]): HomeWire {
  const template = homeWire.agents[0];
  if (template === undefined) throw new Error('the fixture holds no agent');
  const seat = (slot: SessionSlot, label: string): AgentRow => ({
    ...template,
    slot,
    label,
    lifecycle: 'Idle',
    pending: null,
    died: false,
    failed_turn: null,
  });
  return {
    ...homeWire,
    agents: [seat(LEAD, 'lead'), seat(WORKER, 'w1')],
    fleet: [
      {
        project: 'proj',
        live_workers: 2,
        slots: 2,
        queue: 0,
        waiting_on_user: waits.length,
        attention: waits,
        rows: waits.length,
        misses: [],
      },
    ],
  };
}

const WAITS: AttentionRow[] = [
  {
    task: 't1',
    owner: 'lead',
    subject: 'the parser',
    words: 'sign this off',
    at: at(5),
  },
  {
    task: 't2',
    owner: 'lead',
    subject: 'the schema',
    words: 'pick one',
    at: at(9),
  },
  {
    task: 't3',
    owner: 'w1',
    subject: 'the tests',
    words: 'look at these',
    at: at(3),
  },
];

/** The mark the rail draws on a row holding `count` decision waits. */
function mark(count: number): HTMLElement | null {
  const word = count === 1 ? '1 decision wait on you' : `${count} decision waits on you`;
  return document.querySelector<HTMLElement>(`button[aria-label="${word}"]`);
}

describe('the rail on a project holding decision waits', () => {
  it('marks the project and the seat that holds one, and lists the asks behind it', () => {
    app = mount(Rail, {
      target: document.body,
      props: { home: waiting(WAITS), current: ELSEWHERE, now: 0, connection: recording() },
    });
    flushSync();

    const project = mark(3);
    expect(project, 'the project row carries all three waits').not.toBeNull();
    expect(mark(1), "and the worker's own row carries its one").not.toBeNull();

    project?.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    flushSync();
    const words = document.body.textContent ?? '';
    expect(words, 'the popover lists the first ask in its own words').toContain('sign this off');
    expect(words, 'and the second').toContain('pick one');
    expect(words, "and the worker's").toContain('look at these');
    expect(
      document.querySelector('a[href="/session/TestOrg/proj/lead"]'),
      'an item opens the seat that raised it',
    ).not.toBeNull();
  });

  /// **A wait ends when its row leaves waiting** (review focus 2): the mark
  /// is a read of the pushed rows, so the read without the wait drops it -
  /// nobody has to open the seat for it to go.
  it('drops the mark when the board answers the wait', async () => {
    app = mount(Rail, {
      target: document.body,
      props: { home: waiting(WAITS), current: ELSEWHERE, now: 0, connection: recording() },
    });
    flushSync();
    expect(mark(3), 'the wait is marked').not.toBeNull();

    await unmount(app);
    app = mount(Rail, {
      target: document.body,
      props: { home: waiting([]), current: ELSEWHERE, now: 0, connection: recording() },
    });
    flushSync();
    expect(mark(3), 'the answered read draws no mark').toBeNull();
  });
});
