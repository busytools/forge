import { describe, expect, it } from 'vitest';

import { applyFleetCounts, type HomeWire } from './home';

function wire(fleet: HomeWire['fleet']): HomeWire {
  return {
    projects: [],
    agents: [],
    fleet,
    unseen: [],
    accounts: {
      loading: [],
      all_loaded: true,
      gateway: { ready: true, port: 8787, bind_error: null },
      usage: [],
    },
    connectors: {},
    dictate: {} as HomeWire['dictate'],
    voice: {} as HomeWire['voice'],
    cli_version: null,
    forge_version: 'test',
    forge_version_short: 'test',
    boot_id: 'boot',
    service_status: null,
    fatal_error: null,
    biometric: { outcome: 'read', value: [] },
    vault: null,
  };
}

/** One fleet row, the figures a task set can move. */
function row(project: string): HomeWire['fleet'][number] {
  return {
    project,
    live_workers: 2,
    slots: 4,
    queue: 9,
    waiting_on_user: 9,
    attention: [],
    rows: 9,
    misses: [],
  };
}

describe('the fleet row a task frame carries', () => {
  it('counts the ready queue, the waits on the person and the rows', () => {
    const before = wire([row('proj'), row('other')]);
    const after = applyFleetCounts(before, 'proj', [
      { id: 't1', status: 'pending', owner: null, waiting_on: null, reserved: null },
      { id: 't2', status: 'pending', owner: { org: 'o', project: 'proj', label: 'lead' } },
      {
        id: 't3',
        status: 'waiting',
        owner: null,
        waiting_on: {
          kind: 'decision',
          detail: null,
          on: null,
          verification: false,
          parked: false,
        },
      },
      { id: 't4', status: 'in_progress', owner: null, waiting_on: null },
      { id: 't5', status: 'pending', owner: null, waiting_on: null, reserved: 'user' },
    ]);

    const held = after?.fleet.find((entry) => entry.project === 'proj');
    expect(held?.queue, 'only the unowned, unreserved, unwaiting pending row is ready').toBe(1);
    expect(held?.waiting_on_user, 'the decision wait is on the person').toBe(1);
    expect(held?.rows, 'the row count is the set').toBe(5);
    // The server's own derivation is not the frame's to move.
    expect(held?.live_workers, 'a task frame moved the worker count').toBe(2);
    expect(after?.fleet[1], 'another project was touched').toEqual(row('other'));
  });

  it('applies nothing without a snapshot, or to a project it does not hold', () => {
    expect(applyFleetCounts(null, 'proj', []), 'a frame landed before the first read').toBeNull();
    const before = wire([row('proj')]);
    // **A project the draw does not hold has nowhere to apply into**, so the
    // caller's owed read still runs (#3074 review).
    expect(
      applyFleetCounts(before, 'elsewhere', [{ id: 't', status: 'pending' }]),
      'a frame for an unknown project read as applied',
    ).toBeNull();
  });

  it('counts a verification wait on the person only when the row carries its own gate', () => {
    const before = wire([row('proj')]);
    const after = applyFleetCounts(before, 'proj', [
      {
        id: 't1',
        status: 'waiting',
        waiting_on: { kind: 'decision', verification: true, detail: null, on: null },
        verify: 'user',
      },
      {
        id: 't2',
        status: 'waiting',
        waiting_on: { kind: 'decision', verification: true, detail: null, on: null },
        verify: null,
      },
    ]);
    expect(
      after?.fleet[0]?.waiting_on_user,
      'the inherited-gate park was counted as an ask on the person',
    ).toBe(1);
  });
});
