// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import { tasks } from '../chat/tasks.svelte';
import { homeWire } from '../dev/fixture.data';
import { PROTOCOL_VERSION } from '../protocol';
import type { Connection } from '../socket';
import type { HomeWire, TaskStatus } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import SpawnHarness from './SpawnHarness.svelte';

/**
 * The tasks sync as the PAGE drives it: the home moves under a mounted
 * session page, which is what re-runs the effect `Session.svelte` syncs
 * from. The store reads its rows back inside that effect, and a reactive
 * read of state the same run wrote makes the effect its own dependent - the
 * depth guard then ends the page on every frame the home lands. Measured
 * live: a moved task threw `effect_update_depth_exceeded` every second.
 */
const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

/** A home whose project carries one task, in the state named. */
function homeWith(status: TaskStatus): HomeWire {
  const project = homeWire.projects[0];
  if (project === undefined) throw new Error('the fixture holds no project');
  return {
    ...homeWire,
    projects: [
      {
        ...project,
        rows: [
          {
            task: {
              id: 't1',
              project_name: 'proj',
              subject: 'a task',
              active_form: null,
              detail: null,
              status,
              owner: null,
              parent: null,
              waiting_on: null,
              estimate: null,
              rank: null,
              verify: null,
              links: [],
              attempt: 0,
              archived_at: null,
              created_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
              updated_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
            },
            worked_secs: 0,
            updated_secs_ago: 0,
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

/** A connection the page may hold without asking it for anything. */
function quiet(): Connection {
  return {
    subscribe: () => ({
      subject: { session: LEAD },
      value: { subscribe: () => () => {} },
      snapshot: () => null,
      updates: () => [],
      state: () => ({ kind: 'loading' }),
      dropped: () => 0,
      set: () => {},
      push: () => {},
      refuse: () => {},
    }),
    unsubscribe: () => {},
    refresh: () => {},
    dispatch: () => null,
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
    serverProtocol: () => PROTOCOL_VERSION,
    status: () => 'open',
    close: () => {},
  } as unknown as Connection;
}

let app: ReturnType<typeof mount> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  tasks.sync(null);
});

describe('the tasks row under a moving home', () => {
  it('settles when the home moves the task, and lights the row that moved', () => {
    app = mount(SpawnHarness, {
      target: document.body,
      props: { slot: LEAD, connection: quiet(), home: homeWith('in_progress') },
    });
    flushSync();
    expect(
      tasks.rows().map((row) => row.status),
      'the first frame did not land',
    ).toEqual(['in_progress']);

    // The home moves the task, which is what a frame for this seat does.
    const page = (app as unknown as { page: { home: HomeWire } }).page;
    page.home = homeWith('completed');
    flushSync();

    expect(tasks.rows().map((row) => row.status)).toEqual(['completed']);
    expect(tasks.lit('t1'), 'the moved task lit nothing').toBe(true);
  });
});
