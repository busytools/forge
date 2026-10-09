// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import { PROTOCOL_VERSION } from '../protocol';
import type { Connection } from '../socket';
import type { Task } from '../wire/home';
import { DEFAULT_SETTINGS } from '../wire/types';
import Router from './Router.svelte';

/**
 * The board's edits reach the socket.
 *
 * The page builds the command and hands it to the router; the router is
 * what puts it on the connection. This pins that last hop: a control the
 * app forgot to wire draws and does nothing, which reads exactly like a
 * core that refused it.
 */
let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

const LEAD = { org: 'TestOrg', project: 'proj', label: 'lead' };

/** A connection whose `dispatch` records, and whose every other call refuses. */
function recording(sent: Record<string, unknown>[]): { connection: Connection } {
  const refuse = (): never => {
    throw new Error('this render reached a part of the connection it should not have');
  };
  const connection = {
    subscribe: refuse,
    unsubscribe: refuse,
    refresh: refuse,
    dispatch: (command: Record<string, unknown>) => {
      sent.push(command);
      return null;
    },
    more: refuse,
    devices: refuse,
    frame: refuse,
    onBrowserAsk: refuse,
    browserRole: refuse,
    onBrowserRole: refuse,
    takeBrowserRole: refuse,
    onMessage: refuse,
    onStatus: refuse,
    store: refuse,
    settings: refuse,
    skew: refuse,
    serverProtocol: () => PROTOCOL_VERSION,
    status: refuse,
    close: refuse,
  } as unknown as Connection;
  return { connection };
}

/** The fixture's project, with one row waiting on the reader's look. */
function wireWithWaiting(): typeof homeWire {
  const first = homeWire.projects[0];
  if (first === undefined) throw new Error('the fixture holds no project');
  const waiting: Task = {
    id: 't1',
    project_name: 'proj',
    subject: 'a row waiting on the reader',
    active_form: null,
    detail: null,
    status: 'waiting',
    owner: null,
    parent: null,
    waiting_on: { kind: 'decision', detail: null, on: null, verification: true },
    estimate: null,
    rank: null,
    verify: 'user',
    links: [],
    attempt: 1,
    archived_at: null,
    created_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
    updated_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
  };
  return {
    ...homeWire,
    projects: [
      {
        ...first,
        rows: [
          {
            task: waiting,
            worked_secs: 60,
            updated_secs_ago: 60,
            marks: {
              ready: false,
              in_review: false,
              overdue: false,
              no_movement: false,
              waiting_too_long: false,
              stale: false,
              to_close: false,
            },
            rollup: null,
            parent_subject: null,
          },
        ],
      },
    ],
  };
}

function summon(sent: Record<string, unknown>[]): void {
  const { connection } = recording(sent);
  app = mount(Router, {
    target: document.body,
    props: {
      route: { name: 'board', org: 'TestOrg', project: 'proj' },
      settings: DEFAULT_SETTINGS,
      address: '127.0.0.1:8790',
      home: { wire: wireWithWaiting(), refused: null, report: null },
      failure: null,
      connected: true,
      connection,
      notice: null,
      onconnect: () => {},
    },
  });
  flushSync();
}

describe('a board edit through the router', () => {
  it('puts the approve command on the socket with its project', () => {
    const sent: Record<string, unknown>[] = [];
    summon(sent);

    const approve = [...document.querySelectorAll<HTMLElement>('button')].find(
      (button) => button.textContent?.trim() === 'approve',
    );
    if (approve === undefined) throw new Error('the approve control is not on the page');
    approve.click();
    flushSync();

    expect(sent).toEqual([{ task_verdict: { project: 'proj', id: 't1', approve: true } }]);
  });

  it('draws the takeover a board address names', () => {
    summon([]);
    expect(document.querySelector('.b-top')).not.toBeNull();
    expect(document.body.textContent).toContain('proj');
    expect(document.body.textContent).toContain('waiting on you');
    expect(document.body.textContent).toContain(LEAD.label);
  });
});
