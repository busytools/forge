/**
 * The board's closed view over a live connection: the archive's one read.
 *
 * **One read per opening of the toggle, not a subscription.** The archived
 * set is history - a page that keeps it open re-reads only when the toggle
 * is pressed again or the page recovers, and the board's own store already
 * hears the task writes that moved a row onto this shelf. Carrying the set
 * on a stream would hold a growing history on the page for the whole visit.
 */

import type { Connection } from '../socket';
import { rowFrom, type Task } from '../wire/home';

/** One project's closed rows: the terminal live rows, then the archive. */
export interface ProjectArchivedWire {
  project: { org: string; name: string };
  rows: Task[];
}

export interface ArchivedWire {
  projects: ProjectArchivedWire[];
}

/** What the toggle draws: the read, the refusal, or the wait for one. */
export interface ArchivedRead {
  wire: ArchivedWire | null;
  refused: string | null;
}

/** A closed row as the board reads a task: the same narrowing as a row's. */
export function closedTaskFrom(entry: unknown): Task {
  return rowFrom({ task: entry }).task;
}

/** The closed view's snapshot, narrowed where it enters. */
export function archivedFrom(data: unknown): ArchivedWire {
  const held = (typeof data === 'object' && data !== null ? data : {}) as {
    projects?: unknown;
  };
  const projects = Array.isArray(held.projects) ? held.projects : [];
  return {
    projects: projects.flatMap((entry) => {
      const project = (entry as { project?: { org?: unknown; name?: unknown } } | null)?.project;
      if (typeof project?.org !== 'string' || typeof project.name !== 'string') return [];
      const rows = (entry as { rows?: unknown }).rows;
      return [
        {
          project: { org: project.org, name: project.name },
          rows: (Array.isArray(rows) ? rows : []).map(closedTaskFrom),
        },
      ];
    }),
  };
}

/**
 * Ask for the closed view once: resolve on the first snapshot or refusal,
 * and give up rather than hang when the server says nothing.
 */
export function readArchived(connection: Connection): Promise<ArchivedRead> {
  return new Promise((resolve) => {
    let settled = false;
    const store = connection.subscribe('archived');
    let stop: () => void = () => {};
    const finish = (value: ArchivedRead): void => {
      if (settled) return;
      settled = true;
      stop();
      connection.unsubscribe('archived');
      resolve(value);
    };
    stop = store.value.subscribe((held) => {
      if (held.state.kind === 'ready') {
        finish({ wire: archivedFrom(held.snapshot), refused: null });
      } else if (held.state.kind === 'refused') {
        finish({ wire: null, refused: held.state.why });
      }
    });
    setTimeout(() => finish({ wire: null, refused: 'the archive did not answer' }), 8_000);
  });
}
