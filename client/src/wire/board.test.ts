import { describe, expect, it } from 'vitest';

import { boardFrom, coversBoard, type BoardWire } from './board';
import type { SessionSlot } from './types';

const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

/**
 * The board parse is the one place a task's newer fields enter, and a field
 * it drops draws nothing: no mark, no reserved chip, no parked wait. These
 * pin the parse rather than the renderer, so a regression here fails by
 * name instead of quietly losing the board's words (#2423).
 */
function parsedTask(task: Record<string, unknown>): Record<string, unknown> {
  const raw = {
    projects: [{ work: { gate: 'in_repo' }, rows: [{ task }] }],
  } as unknown as BoardWire;
  const taskRow = boardFrom(raw).projects[0]?.rows[0]?.task;
  if (taskRow === undefined) throw new Error('the row did not parse');
  return taskRow as unknown as Record<string, unknown>;
}

describe('the board hears exactly the variants that move it (#2517)', () => {
  it('takes its own three and the seat events its rows read', () => {
    expect(coversBoard({ tasks_changed: { key: LEAD, tasks: [] } })).toBe(true);
    expect(coversBoard({ cron_schedules_changed: { key: LEAD, crons: [] } })).toBe(true);
    expect(
      coversBoard({ connector_subscriptions_changed: { key: LEAD, gotify: [], slack: [] } }),
    ).toBe(true);
    // A row's owner-gone chip is read from seat liveness, so these reach it.
    for (const variant of [
      'spawning',
      'connected',
      'session_replaced',
      'releasing',
      'connection_failed',
      'worker_status_changed',
    ]) {
      expect(coversBoard({ [variant]: { key: LEAD } }), `${variant} was dropped`).toBe(true);
    }
    expect(coversBoard({ accounts_changed: {} })).toBe(false);
    expect(coversBoard('catalog_loaded')).toBe(false);
    // A turn frame moves no board row's marks.
    expect(coversBoard({ turn_complete: { key: LEAD } })).toBe(false);
  });
});

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
    // A payload that states none of the newer fields: no attestation, no
    // reservation, and a wait that is not parked.
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
