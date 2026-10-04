import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import { homeView, type Row as RowModel } from '../home/view';
import type { AgentRow, HomeWire, ProjectWire } from '../wire/home';
import Row from './Row.svelte';

/**
 * A row's name is the way into the seat it names - except where the seat's
 * page refuses: a WORKER's seat with no session behind it. A lead's seat
 * always answers - the core resolves its directory from the project
 * declaration, and opening one starts it - so a sleeping lead keeps its link,
 * which the dormant-project case drives from the view itself rather than from
 * a hand-built state.
 *
 * Read as markup, because whether a link is drawn at all is a question about
 * the tree. The seat's own path is what is asserted, so a row's artifact
 * anchor (an external URL) cannot stand in for it.
 */

/** One agent row, in the fixture's own shape. */
function agentIn(label: string, lifecycle: AgentRow['lifecycle']): AgentRow {
  const template = homeWire.agents[0];
  if (template === undefined) throw new Error('the fixture holds no agent');
  return { ...template, slot: { ...template.slot, label }, label, lifecycle, pending: null };
}

/** The fixture's home, with these agents and this project history. */
function homeWith(
  over: { agents?: AgentRow[]; sessions?: ProjectWire['project']['sessions'] } = {},
): HomeWire {
  const project = homeWire.projects[0];
  if (project === undefined) throw new Error('the fixture holds no project');
  return {
    ...homeWire,
    projects: [
      {
        ...project,
        project: { ...project.project, sessions: over.sessions ?? project.project.sessions },
      },
    ],
    agents: over.agents ?? homeWire.agents,
  };
}

/** The one project's rows, as the view draws them. */
function rowsOf(home: HomeWire): { lead: RowModel; workers: RowModel[] } {
  const entry = homeView(home, '127.0.0.1:8790').orgs[0]?.projects[0];
  if (entry === undefined) throw new Error('the fixture drew no project');
  return entry;
}

/** A worker row of a project whose lead is up. */
function workerIn(lifecycle: AgentRow['lifecycle']): RowModel {
  const worker = rowsOf(homeWith({ agents: [agentIn('lead', 'Idle'), agentIn('w1', lifecycle)] }))
    .workers[0];
  if (worker === undefined) throw new Error('the view drew no worker row');
  return worker;
}

const body = (row: RowModel): string => render(Row, { props: { row, now: 0 } }).body;

describe('a row that names a seat', () => {
  it('links a lead seat, which the core can always answer', () => {
    expect(body(rowsOf(homeWire).lead)).toContain('href="/session/');
  });

  /**
   * The regression the review found: a project nothing has started yet but
   * which ran before draws its lead row `Sleeping`, and that page still
   * answers - opening it starts the lead - so the row keeps its link. The
   * refusal is a worker's seat with no session behind it, and nothing else.
   */
  it('links a dormant-but-previously-run project, whose page starts the lead', () => {
    const lead = rowsOf(
      homeWith({
        agents: [],
        sessions: [{ last_activity: { secs_since_epoch: 1, nanos_since_epoch: 0 } }],
      }),
    ).lead;
    expect(lead.state, 'the view did not draw the dormant path').toEqual({
      kind: 'lifecycle',
      lifecycle: 'Sleeping',
    });
    expect(body(lead)).toContain('href="/session/');
  });

  it('links a project nothing has ever run in, whose page starts the lead', () => {
    const lead = rowsOf(homeWith({ agents: [], sessions: [] })).lead;
    expect(lead.state, 'the view did not draw the never-started path').toEqual({
      kind: 'never-started',
    });
    expect(body(lead)).toContain('href="/session/');
  });

  it('draws a sleeping worker as a label rather than a link, saying so in words', () => {
    const held = body(workerIn('Sleeping'));
    expect(held, 'a sleeping worker was drawn as a way in').not.toContain('href="/session/');
    expect(held, 'the row stopped naming the seat').toContain('w1');
    expect(held, 'the row says nothing about why it is not a link').toContain('>asleep<');
  });

  /**
   * The chrome that says "this goes somewhere" follows the LINK, not the
   * `asleep` mark: a dormant lead is asleep and also openable, so a mark-keyed
   * chrome would draw it clickable but inert. The pair is the property - the
   * linked row carries no `unopenable`, the unlinked one does.
   */
  it('keys the way-in chrome on the link, not on the asleep mark', () => {
    const lead = rowsOf(
      homeWith({
        agents: [],
        sessions: [{ last_activity: { secs_since_epoch: 1, nanos_since_epoch: 0 } }],
      }),
    ).lead;
    const leadBody = body(lead);
    expect(leadBody, 'the dormant path stopped drawing the asleep mark').toContain('row asleep');
    expect(leadBody, 'a linked row lost its way-in chrome to the mark').not.toContain('unopenable');
    expect(body(workerIn('Sleeping')), 'a row with no link kept its way-in chrome').toContain(
      'unopenable',
    );
  });

  /** The state the parked web view groups beside `Sleeping`, on a worker. */
  it('draws a signed-out worker the same way', () => {
    expect(body(workerIn('LoggedOut'))).not.toContain('href="/session/');
  });

  it('links a worker that has a session behind it', () => {
    expect(body(workerIn('Running'))).toContain('href="/session/');
  });
});
