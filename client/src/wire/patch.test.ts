import { describe, expect, it } from 'vitest';

import type { Subject } from '../protocol';
import type { BoardWire } from './board';
import type { HomeWire } from './home';
import { applyPatchFor } from './patch';

const HOME: Subject = 'home';
const BOARD: Subject = 'board';
const SEAT: Subject = { session: { org: 'TestOrg', project: 'proj', label: 'lead' } };

function aHome(): HomeWire {
  return {
    projects: [
      {
        project: { org: 'TestOrg', name: 'proj' },
        work: { branch: 'main', changed: 0, gate: 'in_repo' },
        would_bind: true,
        chip: null,
      },
    ],
    agents: [
      {
        slot: { org: 'TestOrg', project: 'proj', label: 'lead' },
        label: 'lead',
        lifecycle: 'Idle',
      },
    ],
    fleet: [{ project: 'proj', live_workers: 1 }],
    unseen: [],
    accounts: { loading: [] },
    connectors: { gotify: { connected: false }, slack: { connected_workspaces: [] } },
    dictate: { enabled: false, snapshot: null, models_dir: null },
    voice: { enabled: false, models_dir: null },
    cli_version: null,
    forge_version: '1',
    forge_version_short: 'abc',
    boot_id: 'b',
    service_status: null,
    fatal_error: null,
    biometric: { outcome: 'read', value: [] },
    vault: null,
  } as unknown as HomeWire;
}

function aBoard(): BoardWire {
  return {
    projects: [
      {
        project: { org: 'TestOrg', name: 'proj' },
        rows: [{ task: { id: 't1', project_name: 'proj', status: 'pending' } }],
        crons: [],
        connectors: { gotify: [], slack: [] },
      },
    ],
  } as unknown as BoardWire;
}

describe('applying a patch', () => {
  /**
   * **A home project is addressed by org and name**, so a session opening
   * inside its view replaces that project rather than reading as one
   * removed and another added.
   */
  it('keys a home project by its org and name, not the whole view', () => {
    const home = aHome();
    const full = {
      ...home,
      projects: [
        {
          ...home.projects[0],
          project: {
            ...(home.projects[0]?.project as object),
            path: '/p',
            sessions: [],
          },
        },
      ],
    } as unknown as HomeWire;
    const patched = applyPatchFor(HOME, full, {
      projects: [
        {
          op: 'set',
          id: { org: 'TestOrg', name: 'proj' },
          record: {
            ...(full.projects[0] as object),
            project: { ...(full.projects[0]?.project as object), sessions: [{ label: 'w1' }] },
          },
        },
      ],
    }) as HomeWire;
    expect(patched.projects).toHaveLength(1);
    expect((patched.projects[0]?.project as { sessions?: unknown[] }).sessions).toHaveLength(1);
  });

  /** **The home's lists replace by id and removals take an id out.** */
  it('sets, replaces and removes home entries', () => {
    const home = aHome();
    const renamed = applyPatchFor(HOME, home, {
      agents: [
        {
          op: 'set',
          id: { org: 'TestOrg', project: 'proj', label: 'lead' },
          record: {
            slot: { org: 'TestOrg', project: 'proj', label: 'lead' },
            label: 'renamed',
            lifecycle: 'Running',
          },
        },
      ],
    }) as HomeWire;
    expect(renamed.agents[0]?.label).toBe('renamed');
    expect(renamed.agents).toHaveLength(1);

    const added = applyPatchFor(HOME, home, {
      agents: [
        {
          op: 'set',
          id: { org: 'TestOrg', project: 'proj', label: 'w1' },
          record: {
            slot: { org: 'TestOrg', project: 'proj', label: 'w1' },
            label: 'w1',
            lifecycle: 'Idle',
          },
        },
      ],
    }) as HomeWire;
    expect(added.agents.map((agent) => agent.label)).toEqual(['lead', 'w1']);

    const removed = applyPatchFor(HOME, home, {
      projects: [{ op: 'removed', id: { org: 'TestOrg', name: 'proj' } }],
    }) as HomeWire;
    expect(removed.projects).toEqual([]);
  });

  /** Every other field is replaced whole, and a cleared optional crosses as
   * the record's own `null`. */
  it('replaces whole fields and clears an optional one', () => {
    const home = aHome();
    const patched = applyPatchFor(HOME, home, {
      cli_version: { installed: '2', latest: '2' },
      service_status: null,
      vault: { locked: true, idle_minutes: 5 },
    }) as HomeWire;
    expect(patched.cli_version).toEqual({ installed: '2', latest: '2' });
    expect(patched.service_status).toBeNull();
    expect(patched.vault).toEqual({ locked: true, idle_minutes: 5 });

    const cleared = applyPatchFor(
      HOME,
      { ...home, cli_version: { installed: '1', latest: '1' } },
      { cli_version: null },
    ) as HomeWire;
    expect(cleared.cli_version).toBeNull();
  });

  /** **The board's rows travel by task id inside their project's entry**, and
   * a project the client never read arrives with its whole board. */
  it('sets and removes board rows inside a project entry', () => {
    const board = aBoard();
    const patched = applyPatchFor(BOARD, board, {
      projects: [
        {
          op: 'set',
          identity: { org: 'TestOrg', name: 'proj' },
          board: {
            rows: [
              {
                op: 'set',
                id: 't1',
                record: { task: { id: 't1', project_name: 'proj', status: 'completed' } },
              },
              {
                op: 'set',
                id: 't2',
                record: { task: { id: 't2', project_name: 'proj', status: 'pending' } },
              },
            ],
          },
        },
      ],
    }) as BoardWire;
    const rows = patched.projects[0]?.rows ?? [];
    expect(rows.map((row) => row.task.id)).toEqual(['t1', 't2']);
    expect(rows[0]?.task.status).toBe('completed');

    const removed = applyPatchFor(BOARD, board, {
      projects: [
        {
          op: 'set',
          identity: { org: 'TestOrg', name: 'proj' },
          board: { rows: [{ op: 'removed', id: 't1' }] },
        },
      ],
    }) as BoardWire;
    expect(removed.projects[0]?.rows).toEqual([]);

    const fresh = applyPatchFor(
      BOARD,
      { projects: [] },
      {
        projects: [
          {
            op: 'set',
            identity: { org: 'TestOrg', name: 'proj' },
            board: { rows: [], crons: [], connectors: { gotify: [], slack: [] } },
          },
        ],
      },
    ) as BoardWire;
    expect(fresh.projects[0]?.project).toEqual({ org: 'TestOrg', name: 'proj' });
  });

  /** **A seat's patch is whole fields**, the same form the small records
   * use - no entries, no ids. */
  it('replaces a seat field whole', () => {
    const record = {
      slot: SEAT.session,
      composer: { machine: null, bind: 'hold', mode: 'toggle', notice: null },
      reviews: { threads: { outcome: 'unknown', value: 'absent' } },
    };
    const patched = applyPatchFor(SEAT, record, {
      composer: { machine: 'compacting', bind: 'hold', mode: 'toggle', notice: null },
    }) as typeof record;
    expect(patched.composer.machine).toBe('compacting');
    expect(patched.reviews).toEqual(record.reviews);

    const small = applyPatchFor(
      'extensions',
      { snapshot: null, update_run: null, lsp: [] },
      {
        lsp: [{ server: 'rust-analyzer', on_path: true }],
      },
    ) as { lsp: unknown[] };
    expect(small.lsp).toHaveLength(1);
  });

  /** **A patch this client cannot read comes back `null`**, so the socket can
   * hold it and ask for one whole read rather than dropping it (rule 25). */
  it('refuses a shape it cannot read', () => {
    expect(applyPatchFor(HOME, null, { cli_version: null })).toBeNull();
    expect(applyPatchFor(BOARD, aBoard(), { projects: 'not a list' })).toBeNull();
    expect(applyPatchFor(HOME, aHome(), { agents: [{ op: 'set' }] })).toBeNull();
    expect(applyPatchFor(HOME, aHome(), { projects: [{ op: 'weird' }] })).toBeNull();
    expect(applyPatchFor('dictate', null, {})).toBeNull();
  });
});
