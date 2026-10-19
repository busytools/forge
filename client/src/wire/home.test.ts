import { describe, expect, it } from 'vitest';

import { homeFrom, type HomeWire } from './home';

/**
 * The home parse is the one place a task's newer fields enter, and a field
 * it drops draws nothing: no mark, no reserved chip, no parked wait. These
 * pin the parse rather than the renderer, so a regression here fails by
 * name instead of quietly losing the board's words (#2423).
 */
function parsedTask(task: Record<string, unknown>): Record<string, unknown> {
  const raw = {
    service_status: null,
    fatal_error: null,
    agents: [],
    projects: [{ work: { gate: 'in_repo' }, rows: [{ task }] }],
    fleet: [],
    accounts: { loading: [] },
    dictate: { snapshot: { models: [] } },
  } as unknown as HomeWire;
  const taskRow = homeFrom(raw).projects[0]?.rows[0]?.task;
  if (taskRow === undefined) throw new Error('the row did not parse');
  return taskRow as unknown as Record<string, unknown>;
}

describe('the task fields the board marks', () => {
  it('keeps the attestation, the reservation and a parked wait', () => {
    const task = parsedTask({
      id: 't-1',
      project_name: 'proj',
      subject: 'a look',
      status: 'completed',
      attested: {
        evidence: 'Looks right - merge',
        at: { secs_since_epoch: 1, nanos_since_epoch: 0 },
      },
      reserved: 'user',
      waiting_on: { kind: 'decision', detail: null, on: null, verification: false, parked: true },
    });
    const attested = task['attested'] as { evidence?: string } | null;
    expect(attested?.evidence, 'the quoted approval reached the row').toBe('Looks right - merge');
    expect(task['reserved'], 'the reserved pick reached the row').toBe('user');
    const wait = task['waiting_on'] as { parked?: boolean } | null;
    expect(wait?.parked, 'the parked flag reached the wait').toBe(true);
  });

  it('defaults absent newer fields rather than dropping the row', () => {
    // A server older than the fields, and a v7 flat task alike: no attestation,
    // no reservation, and a wait that is not parked.
    const task = parsedTask({
      id: 't-2',
      project_name: 'proj',
      subject: 'an older row',
      status: 'waiting',
      waiting_on: { kind: 'decision', detail: null, on: null, verification: false },
    });
    expect(task['attested']).toBeNull();
    expect(task['reserved']).toBeNull();
    const wait = task['waiting_on'] as { parked?: boolean } | null;
    expect(wait?.parked, 'an older server sent no parked flag').toBe(false);
  });
});
