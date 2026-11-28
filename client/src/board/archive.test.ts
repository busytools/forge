import { describe, expect, it } from 'vitest';

import { archivedFrom } from './archive';

describe('the closed view as the toggle narrows it', () => {
  it('reads each project with its rows', () => {
    const wire = archivedFrom({
      projects: [
        {
          project: { org: 'TestOrg', name: 'proj' },
          rows: [{ id: 't1', project_name: 'proj', subject: 'a closed row', status: 'completed' }],
        },
      ],
    });
    expect(wire.refused).toBeNull();
    expect(wire.projects).toHaveLength(1);
    expect(wire.projects[0]?.rows.map((row) => row.id)).toEqual(['t1']);
  });

  it('draws a malformed project entry as a failure, not a shorter shelf', () => {
    const wire = archivedFrom({
      projects: [{ project: { org: 'TestOrg', name: 'proj' }, rows: [] }, { rows: [] }],
    });
    expect(wire.refused).toBe('the archive carried a project this client could not read');
    expect(wire.projects.map((project) => project.project.name)).toEqual(['proj']);
  });

  it("keeps the server's own refusal as the refusal", () => {
    const wire = archivedFrom({ refused: 'the task archive could not be read: disk' });
    expect(wire.refused).toBe('the task archive could not be read: disk');
  });
});
