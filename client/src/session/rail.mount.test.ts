// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import type { Command } from '../protocol';
import type { Connection } from '../socket';
import type { AgentRow, HomeWire } from '../wire/home';
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
    release: refuse,
    dispatch: (command: Command) => {
      acted.push(command);
      return null;
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
