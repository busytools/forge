import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import type {
  AgentRow,
  BoardRow,
  DictateModel,
  FleetRow,
  HomeWire,
  Lifecycle,
  ProjectWire,
  Task,
  TaskStatus,
  WireTime,
  WorkState,
} from '../wire/home';
import { homeFrom, missFrom } from '../wire/home';

describe('an agent row from a server that names no failure', () => {
  /**
   * The fold's own `?? null`: a server old enough not to state the failure
   * leaves the field out, and it must read as `null` rather than as a
   * failure. The only pin of this branch went with the floor fixture, and it
   * matters more now - a server on another protocol connects and is folded
   * rather than refused.
   */
  it('reads an absent failed_turn as null rather than as a failure', () => {
    const raw = {
      projects: [],
      agents: [
        {
          slot: { org: 'O', project: 'P', label: 'lead' },
          label: 'lead',
          lifecycle: 'Idle',
          has_background_work: false,
          pending: null,
          pending_depth: 0,
          last_activity: null,
          reason: null,
          work: null,
        },
      ],
      unseen: [],
      accounts: {
        loading: [],
        all_loaded: true,
        gateway: { ready: true, port: 0, bind_error: null },
        usage: [],
        orgs: [],
      },
      plugins: { update_records: [] },
      workers: [],
      connectors: null,
      dictate: { enabled: false, snapshot: { models: [], failure: null }, models_dir: null },
      cli_version: null,
      forge_version: '1.0.0',
      forge_version_short: '1.0.0',
      service_status: null,
      fatal_error: null,
    };
    expect(
      homeFrom(raw as unknown as HomeWire).agents[0]?.failed_turn,
      'an absent failure reads as null',
    ).toBeNull();
  });
});
import {
  artifactLabel,
  availableVersion,
  chipFor,
  elapsedLabel,
  fleetRows,
  gateLine,
  failureFile,
  failureKind,
  followable,
  homeView,
  markOf,
  modelState,
  projectRows,
  refusal,
  whenOf,
  type ProjectRows,
  type Row,
  type RowState,
} from './view';

/**
 * Every state a row can draw and the class it carries.
 *
 * `Sleeping` and `LoggedOut` deliberately share `asleep`: both are a session
 * that is not there, and the row says the same thing about each.
 */
const MARKS: [RowState, string][] = [
  [{ kind: 'lifecycle', lifecycle: 'Running' }, 'running'],
  [{ kind: 'lifecycle', lifecycle: 'Spawning' }, 'spawning'],
  [{ kind: 'lifecycle', lifecycle: 'Idle' }, 'idle'],
  [{ kind: 'lifecycle', lifecycle: 'Attention' }, 'needs'],
  [{ kind: 'lifecycle', lifecycle: 'AuthRequired' }, 'auth'],
  [{ kind: 'lifecycle', lifecycle: 'Failed' }, 'failed'],
  [{ kind: 'lifecycle', lifecycle: 'Sleeping' }, 'asleep'],
  [{ kind: 'lifecycle', lifecycle: 'LoggedOut' }, 'asleep'],
  [{ kind: 'unseen' }, 'unseen'],
  [{ kind: 'never-started' }, 'never'],
];

describe('the row marks', () => {
  it('draws the class its state names, one per meaning', () => {
    for (const [state, expected] of MARKS) {
      expect(markOf(state).class, `${JSON.stringify(state)} draws ${expected}`).toBe(expected);
      expect(markOf(state).dot, `${JSON.stringify(state)} draws a dot`).not.toBe('');
    }
    // Nothing the sheet styles is unreachable, and nothing drawn is unstyled.
    // Read from the sheet rather than a list here, which could only ever fail
    // on a rename inside this table - the inverse of what it is for.
    const sheet = readFileSync(new URL('../assets/web.css', import.meta.url), 'utf8');
    const styled = new Set([...sheet.matchAll(/\.row\.([a-z-]+)/g)].map((match) => match[1]));
    const drawn = new Set(MARKS.map(([, klass]) => klass));
    expect([...styled].sort(), 'the sheet styles a row state nothing draws').toEqual(
      [...drawn].sort(),
    );
  });
});

/** A fleet wide enough to reach the shapes one fixture cannot: two orgs, two projects in one of them, two agents in one project. */
const FLEET: HomeWire = {
  ...homeWire,
  cli_version: { installed: '2.1.280', latest: '2.1.290' },
  projects: [
    project('Busytools', 'forge'),
    project('Busytools', 'notes'),
    project('Personal', 'dotfiles'),
  ],
  agents: [
    agent('Busytools', 'forge', 'lead', 'Running'),
    agent('Busytools', 'forge', 'w1', 'Idle'),
    agent('Busytools', 'notes', 'lead', 'Sleeping'),
    agent('Personal', 'dotfiles', 'lead', 'Idle'),
  ],
  // The glance, one row per project: the fleet row names only its PROJECT,
  // and the org it draws comes from the projects list beside it.
  fleet: [fleetRow('forge', 2), fleetRow('notes', 1), fleetRow('dotfiles', 1)],
};

/** One fleet row's counts, whose project the fixture's orgs resolve. */
function fleetRow(project: string, live: number): FleetRow {
  return {
    project,
    live_workers: live,
    slots: 2,
    queue: 0,
    waiting_on_user: 0,
    misses: [],
  };
}

/** One board row for a task, with no derived facts - what a test overrides. */
function boardRow(task: Task): BoardRow {
  return {
    task,
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
  };
}

/**
 * One project row: the project, and the per-row reads the home draws it
 * from. A test still passes `tasks` (the records it reasons about); the
 * helper wraps them as board rows, which is the wire's own shape.
 */
function project(
  org: string,
  name: string,
  over: Partial<ProjectWire> & { tasks?: Task[] } = {},
): ProjectWire {
  const { tasks = [], ...rest } = over;
  return {
    project: {
      key: `${org}-${name}`,
      name,
      org,
      path: `/p/${name}`,
      display_path: `/p/${name}`,
      accounts: ['Acct'],
      fallback_accounts: [],
      has_model: true,
      sessions: [],
    },
    work: { branch: null, changed: null, gate: 'in_repo' },
    rows: tasks.map(boardRow),
    crons: [],
    connectors: { gotify: [], slack: [] },
    would_bind: true,
    chip: null,
    ...rest,
  };
}

/** One task, held by a project's lead. */
function task(status: TaskStatus, subject: string): Task {
  return {
    id: subject,
    project_name: 'proj',
    subject,
    active_form: null,
    detail: null,
    status,
    owner: { org: 'TestOrg', project: 'proj', label: 'lead' },
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
  };
}

function agent(
  org: string,
  project: string,
  label: string,
  lifecycle: Lifecycle,
  work: WorkState | null = null,
  failedTurn: WireTime | null = null,
): AgentRow {
  return {
    slot: { org, project, label },
    label,
    lifecycle,
    has_background_work: false,
    pending: null,
    pending_depth: 0,
    last_activity: null,
    reason: null,
    failed_turn: failedTurn,
    work,
  };
}

/**
 * The first of a list, typed. `noUncheckedIndexedAccess` makes every `[0]` a
 * `T | undefined`, and a spread of one is a partial row rather than the
 * declared type; a cast would say the same thing less honestly.
 */
function first<T>(items: T[], what: string): T {
  const [head] = items;
  if (head === undefined) throw new Error(`the fixture carries no ${what}`);
  return head;
}

/**
 * A project's own rows, from the wire: the lead, and its workers under it.
 *
 * The home's page draws the FLEET (one row per project), and these are what
 * its board and session pages read a project's seats from - so the fixtures
 * below reach them through the same function those pages do rather than
 * through a view shape no production page holds.
 */
function rowsOf(wire: HomeWire, at = 0): ProjectRows {
  return projectRows(wire, first(wire.projects.slice(at), 'project'));
}

/** The first project's lead row, which every fixture here draws. */
function leadOf(wire: HomeWire): Row {
  return rowsOf(wire).lead;
}

const PROJECT = first(homeWire.projects, 'project');
const AGENT = first(homeWire.agents, 'agent');
const LOADING = first(homeWire.accounts.loading, 'account');
const FLEET_AGENT = first(FLEET.agents, 'agent');

/** One complete row, so a test that varies one field needs no cast to say so. */
const LEAD_ROW: Row = {
  slot: { org: 'TestOrg', project: 'proj', label: 'lead' },
  state: { kind: 'lifecycle', lifecycle: 'Idle' },
  name: 'proj',
  place: { branch: null, files: null },
  gate: null,
  task: null,
  pending: null,
  reason: null,
  lastActivity: null,
  failedTurn: null,
};

describe('the fleet the snapshot describes', () => {
  /**
   * One fixture is one project with one agent, so a second org, a second
   * project in an org and any worker row are unreachable. Two readings go
   * wrong on a real fleet and none of them on the fixture: the name a worker
   * row carries, and the header's two numbers swapped.
   */
  it('reads every project of every org, and each one its own row', () => {
    const fleet = fleetRows(FLEET);
    expect(fleet.map((row) => `${row.org}/${row.name}`)).toEqual([
      'Busytools/forge',
      'Busytools/notes',
      'Personal/dotfiles',
    ]);
  });

  it('names a worker row for the worker and only the lead for its project', () => {
    const forge = rowsOf(FLEET);
    expect(forge.lead.name).toBe('forge');
    expect(forge.workers.map((worker) => worker.name)).toEqual(['w1']);
    expect(forge.workers[0]?.slot.label).toBe('w1');
  });

  it('counts agents and projects as themselves, not one as the other', () => {
    const header = homeView(FLEET, '').header;
    expect(header.liveAgents, 'the header read the project count as agents').toBe(4);
    expect(header.projects, 'the header read the agent count as projects').toBe(3);
  });

  /**
   * The total is the fleet's: summed over every project the snapshot carries,
   * which is what the server's own home summed. Reading it off the rows on
   * screen instead would count a subset, and the header draws one number for
   * the whole fleet.
   */
  /**
   * The fleet's misses are named, one per shape the wire sends: a stalled
   * queue, a worker holding no row, and a shape this client is older than -
   * which reads as `unknown` rather than vanishing, because a miss nobody
   * can see is the failure this board exists to end.
   */
  it('names each fleet miss', () => {
    const wire: HomeWire = {
      ...homeWire,
      fleet: [
        {
          project: 'proj',
          live_workers: 1,
          slots: 2,
          queue: 3,
          waiting_on_user: 1,
          misses: [
            { kind: 'stalled', label: 'queue is stalling' },
            { kind: 'no-row', label: 'w1 holds no row' },
            { kind: 'unknown', label: 'a miss this client does not know' },
          ],
        },
      ],
    };
    const rows = fleetRows(wire);
    expect(rows).toHaveLength(1);
    // The named spots come first, then the project's own refusal - the
    // fixture declares no model, so the row says a spawn here would be
    // refused rather than dropping what the row drew before the fleet.
    expect(rows[0]?.misses.map((miss) => miss.kind)).toEqual([
      'stalled',
      'no-row',
      'unknown',
      'unknown',
    ]);
    expect(rows[0]?.misses[3]?.label).toContain('no model declared');
    expect(rows[0]?.onYou).toBe(1);
    // And the raw wire's own shapes narrow to those names.
    expect(missFrom('stalled_queue')).toEqual({ kind: 'stalled', label: 'queue is stalling' });
    expect(missFrom({ unaccounted_worker: 'w1' })).toEqual({
      kind: 'no-row',
      label: 'w1 holds no row',
    });
    expect(missFrom(7).kind).toBe('unknown');
  });

  it('totals the tasks across every project, not the ones a row happens to show', () => {
    const wire: HomeWire = {
      ...homeWire,
      projects: [
        project('Busytools', 'forge', {
          tasks: [task('in_progress', 'a'), task('completed', 'b')],
        }),
        project('Busytools', 'notes', { tasks: [task('pending', 'c')] }),
        project('Personal', 'dotfiles'),
      ],
      agents: [],
    };

    expect(homeView(wire, '').header.tasks, "the total is not the fleet's").toBe(3);
  });

  /**
   * A row's `where` is its OWN tree, a worker's included.
   *
   * `ProjectWire.work` is one read built from the lead's seat, so drawing it
   * on a worker's row puts the project's branch under the worker's name - and
   * `main` there reads as the worker's branch rather than as something
   * missing, which is why this page blanked those rows instead. The seat's
   * own read crosses on the row now, so every started row draws its own and
   * the blank is left to the seats that really have no tree.
   */
  it("draws each row's own tree, a worker's included", () => {
    const wire: HomeWire = {
      ...homeWire,
      projects: [
        project('Busytools', 'forge', {
          work: { branch: 'main', changed: 3, gate: 'in_repo' },
        }),
      ],
      agents: [
        agent('Busytools', 'forge', 'lead', 'Running', {
          branch: 'main',
          changed: 3,
          gate: 'in_repo',
        }),
        agent('Busytools', 'forge', 'w1', 'Idle', {
          branch: 'worktree-em-dash-sweep',
          changed: 1,
          gate: 'in_repo',
        }),
        // A seat forge holds no directory for, which is a despawned worker's
        // row rather than a missing read.
        agent('Busytools', 'forge', 'w2', 'Idle'),
      ],
    };

    const entry = rowsOf(wire);
    expect(entry.lead.place, "the lead's own tree is the read").toEqual({
      branch: 'main',
      files: '3 files',
    });
    expect(first(entry.workers, 'worker').place, "a worker drew its project's tree").toEqual({
      branch: 'worktree-em-dash-sweep',
      files: '1 file',
    });
    expect(entry.workers[1]?.place, 'a seat with no tree borrowed one').toEqual({
      branch: null,
      files: null,
    });
    expect(entry.workers[1]?.gate, 'and says nothing about a tree it does not have').toBeNull();

    // A project nobody has started has no seat to read, so its row keeps the
    // project's own read: the cell is filled for a project that has never run.
    const dormant = rowsOf({ ...wire, agents: [] });
    expect(dormant.lead.place, "a dormant project's row lost the project's own read").toEqual({
      branch: 'main',
      files: '3 files',
    });
  });
});

describe('a row over the fleet', () => {
  it('names the lead row for the project', () => {
    const view = homeView(homeWire, '127.0.0.1:8790');
    expect(view.fleet).toHaveLength(1);
    // The lead's label is its identity, not what the row is called here.
    expect(leadOf(homeWire).name).toBe('proj');
    expect(leadOf(homeWire).slot.label).toBe('lead');
  });

  it('reads the fixture the server pinned, with only the ask promoted', () => {
    const view = homeView(homeWire, 'ws://127.0.0.1:8790/socket');
    const lead = rowsOf(homeWire).lead;
    // The fixture's lead holds a permission prompt beside an Idle lifecycle -
    // a pairing this fixture was hand-made with - and the ask is what the row
    // reads (#1885).
    expect(lead?.state).toEqual({ kind: 'lifecycle', lifecycle: 'Attention' });
    expect(lead?.pending).toBe('permission');
    expect(view.header).toEqual({
      liveAgents: 1,
      tasks: 0,
      projects: 1,
      installed: '1.0.0',
      update: '1.1.0',
      // The forge build serving the socket, which the header draws rather
      // than this app's own version.
      version: '<forge-version-short>',
    });
  });

  it('draws the band with the address a reader recognises, not the socket URL', () => {
    const view = homeView(homeWire, 'ws://127.0.0.1:8790/socket');
    const web = view.band.find((card) => card.title === 'web');
    expect(web?.value).toBe('127.0.0.1:8790');
    expect(web?.detail).toBe('this page');
    expect(view.band.map((card) => card.title)).toEqual([
      'gateway',
      'web',
      'dictation',
      'accounts',
    ]);
  });

  it('draws a project nothing has ever run in as never-started rather than asleep', () => {
    const withSessions = (sessions: { last_activity: WireTime | null }[]): HomeWire => ({
      ...homeWire,
      agents: [],
      projects: [{ ...PROJECT, project: { ...PROJECT.project, sessions } }],
    });

    const lead = leadOf(withSessions([]));
    expect(lead.state).toEqual({ kind: 'never-started' });
    expect(whenOf(lead, Date.now())).toBe('never');

    // The same project with a session behind it is asleep instead: a forge
    // restart leaves every project in that case, so reading the lifecycle
    // alone would call the whole fleet new.
    expect(
      rowsOf(withSessions([{ last_activity: { secs_since_epoch: 0, nanos_since_epoch: 0 } }])).lead
        .state,
    ).toEqual({ kind: 'lifecycle', lifecycle: 'Sleeping' });
  });

  it('promotes a backgrounded task to running, and leaves idle alone without one', () => {
    // The fixture's lead holds a permission prompt, which outranks this
    // promotion: this case is about the backgrounded task alone.
    const wire = (has_background_work: boolean): HomeWire => ({
      ...homeWire,
      agents: [{ ...AGENT, pending: null, has_background_work }],
    });
    expect(rowsOf(wire(true)).lead.state).toEqual({
      kind: 'lifecycle',
      lifecycle: 'Running',
    });
    expect(rowsOf(wire(false)).lead.state).toEqual({
      kind: 'lifecycle',
      lifecycle: 'Idle',
    });
  });

  it('draws an empty fleet as the empty state rather than a broken page', () => {
    expect(fleetRows({ ...homeWire, projects: [], agents: [], fleet: [] })).toEqual([]);
  });

  it('names the two refusals, and neither when a spawn would run', () => {
    expect(refusal(false, true)).toBe('no model declared - add `model` to this project');
    expect(refusal(true, false)).toBe('no usable accounts');
    expect(refusal(true, true)).toBeNull();
  });
});

describe('the cells the reshape made drawable', () => {
  /** One project of the fixture fleet, with its row's reads varied. */
  const withRow = (over: Partial<ProjectWire> & { tasks?: Task[] }): HomeWire => {
    // A test reasons about task records; the wire carries board rows.
    const { tasks = [], ...rest } = over;
    const base = tasks.length > 0 ? { ...PROJECT, rows: tasks.map(boardRow) } : PROJECT;
    return { ...homeWire, projects: [{ ...base, ...rest }] };
  };

  /**
   * The fixture's one agent, with ITS row's reads varied. The tree a row
   * draws is the seat's own, so it is varied here rather than on the project
   * above it.
   */
  const withAgentRow = (over: Partial<AgentRow>): HomeWire => ({
    ...homeWire,
    agents: [{ ...AGENT, ...over }],
  });

  /**
   * A count of zero is not a fact about the tree, so it draws nothing. A
   * `changed` of 0 rendered as `0 files` would say a project has moved
   * nothing, which is what the cell already means when it is empty.
   */
  it('draws the branch and the count, and nothing for a count of zero', () => {
    const place = (changed: number | null) =>
      leadOf(withAgentRow({ work: { branch: 'main', changed, gate: 'in_repo' } })).place;
    expect(place(3)).toEqual({ branch: 'main', files: '3 files' });
    expect(place(1)).toEqual({ branch: 'main', files: '1 file' });
    expect(place(0)).toEqual({ branch: 'main', files: null });
    expect(place(null)).toEqual({ branch: 'main', files: null });
  });

  /**
   * The task a row shows is the one furthest from done, not the first the
   * store returned: a label reused by a new worker can hold the last
   * occupant's finished task, and the row would show it under a state column
   * saying running.
   */
  it('shows the task furthest from done, not the first held', () => {
    const held = leadOf(
      withRow({ tasks: [task('completed', 'shipped'), task('in_progress', 'now')] }),
    );
    expect(held.task?.subject).toBe('now');
    expect(held.task?.chip).toBe('in progress');
  });

  /**
   * The unseen mark is the one thing telling you a seat you are not looking
   * at has finished something, and nothing in the records reconstructs it.
   * A row reading the lifecycle alone would draw every settled seat as idle.
   */
  it('draws a seat whose turn finished unwatched as unseen', () => {
    const slot = { org: 'TestOrg', project: 'proj', label: 'lead' };
    const idle = { ...PROJECT, project: { ...PROJECT.project } };
    // No ask on this seat: the case is the unwatched turn alone, and a
    // pending prompt outranks the diamond (#1885).
    const marked: HomeWire = {
      ...homeWire,
      projects: [idle],
      agents: [{ ...AGENT, pending: null }],
      unseen: [slot],
    };

    expect(leadOf(marked).state).toEqual({ kind: 'unseen' });
    // The same seat with nothing unseen is idle, so the mark is the list
    // rather than the lifecycle.
    expect(leadOf({ ...marked, unseen: [] }).state).toEqual({
      kind: 'lifecycle',
      lifecycle: 'Idle',
    });
  });

  /**
   * A failed turn is the row saying "look at this", and the terminal draws
   * it over both the spinner and the unseen diamond. A row reading the
   * lifecycle alone would draw the seat as idle and say nothing.
   */
  it('marks a seat whose newest turn failed, over every other promotion', () => {
    const at = { secs_since_epoch: 1_800_000_000, nanos_since_epoch: 0 };
    const failed = leadOf(withAgentRow({ failed_turn: at }));

    expect(failed.state).toEqual({ kind: 'failed-turn' });
    expect(failed.failedTurn).toEqual(at);
    expect(markOf(failed.state)).toEqual({ class: 'failed', dot: 'bad' });

    // Over the spinner a backgrounded task promotes and over the diamond a
    // completion promotes: the failure is the thing to act on.
    const slot = { org: 'TestOrg', project: 'proj', label: 'lead' };
    const busy: HomeWire = {
      ...homeWire,
      agents: [{ ...AGENT, failed_turn: at, has_background_work: true }],
      unseen: [slot],
    };
    expect(leadOf(busy).state).toEqual({ kind: 'failed-turn' });
  });

  /**
   * A project whose tree is not there says so rather than showing a blank
   * `where` cell that reads as "no branch".
   */
  it('says why a row has no branch to show', () => {
    const row = leadOf(withAgentRow({ work: { branch: null, changed: null, gate: 'gone' } }));
    expect(gateLine('gone')).toBe('its working directory is not there');
    // The gate line is what the row's `what` cell falls back to, so a row
    // with no pending, no task and no refusal still says something.
    expect(row.task).toBeNull();
    expect(gateLine('in_repo'), 'a repository is the row that explains nothing').toBeNull();
  });
});

describe('the update notice', () => {
  /**
   * Both sides have to resolve and the published one has to be strictly
   * newer. A probe that resolved only one side cannot say whether an update
   * is available, and a token that will not parse is not a version.
   */
  it('names the published version only when it is strictly newer', () => {
    expect(availableVersion('2.1.280', '2.1.290')).toBe('2.1.290');
    expect(availableVersion('2.0.99', '2.1.0')).toBe('2.1.0');
    expect(availableVersion('2.1.280', '2.1.280')).toBeNull();
    expect(availableVersion('2.1.290', '2.1.280')).toBeNull();
    expect(availableVersion(null, '2.1.290')).toBeNull();
    expect(availableVersion('2.1.280', null)).toBeNull();
    expect(availableVersion('2.1.280', 'latest')).toBeNull();
    expect(availableVersion('2.1.280', '2.x.0')).toBeNull();
  });

  it('compares numbers rather than the strings', () => {
    expect(availableVersion('2.1.9', '2.1.10')).toBe('2.1.10');
  });

  /**
   * A prerelease or build suffix comes off the patch component and not the
   * rest of it: reading the token whole would make npm's own `2.1.290-pre.1`
   * unparseable, so a prerelease would never draw a notice at all.
   */
  it('reads past a prerelease or build suffix on the patch', () => {
    expect(availableVersion('2.1.280', '2.1.290-pre.1')).toBe('2.1.290-pre.1');
    expect(availableVersion('2.1.280', '2.1.290+build.7')).toBe('2.1.290+build.7');
    // Two names for one triple are not an update, which is the port's own
    // rule: the suffix comes off before the comparison, so a prerelease of
    // the version already installed draws nothing.
    expect(availableVersion('2.1.290-pre.1', '2.1.290')).toBeNull();
  });

  it('carries the answer on the header', () => {
    const header = (installed: string, latest: string) =>
      homeView({ ...homeWire, cli_version: { installed, latest } }, '').header;
    expect(header('2.1.280', '2.1.290').update).toBe('2.1.290');
    expect(header('2.1.280', '2.1.280').update).toBeNull();
  });
});

describe('the dictation card', () => {
  /**
   * Two of the seven states cross as objects rather than strings, so a
   * first-run download would otherwise render `pending, [object Object]` on
   * the front page.
   */
  it('words every model state the way the server words it', () => {
    expect(modelState('pending')).toBe('waiting');
    expect(modelState({ downloading: { downloaded: 1, total: 2, resumed_from: null } })).toBe(
      'fetching',
    );
    expect(modelState('verifying')).toBe('verifying');
    expect(modelState('fetched')).toBe('fetched');
    expect(modelState('loading')).toBe('loading');
    expect(modelState('ready')).toBe('loaded');
    expect(modelState({ failed: { other: { message: 'x' } } })).toBe('failed');
  });

  /**
   * A hash mismatch names the file whose bytes are wrong, and a preflight
   * the user stopped says so rather than sending them after a network
   * problem that does not exist.
   */
  it('names the file a mismatch is about, and reads a stop as a stop', () => {
    const mismatch = {
      hash_mismatch: { path: '/models/s1-mini-f16.gguf', expected: 'a', actual: 'b', size: 1 },
    };
    expect(failureKind(mismatch)).toBe('hash mismatch');
    expect(failureFile(mismatch)).toBe('s1-mini-f16.gguf');

    const cancelled = { cancelled: { kept: 1, total: 2 } };
    expect(failureKind(cancelled)).toBe('failed');
    expect(failureFile(cancelled)).toBe('stopped');

    expect(failureFile({ other: { message: 'the disk is full' } })).toBe('the disk is full');
  });

  it('draws the card from the words rather than the wire tokens', () => {
    const band = (dictate: HomeWire['dictate']) => homeView({ ...homeWire, dictate }, '').band;
    const card = (dictate: HomeWire['dictate']) =>
      band(dictate).find((entry) => entry.title === 'dictation');

    // The committed fixture: two models, both pending.
    expect(card(homeWire.dictate)?.detail).toBe('waiting, waiting');
    expect(card(homeWire.dictate)?.value).toBe('0 of 2 loaded');

    const ready = {
      snapshot: {
        models: [
          { role: 'transcribing', file: 'a.gguf', state: 'ready' as const },
          { role: 'normalization', file: 'b.gguf', state: 'ready' as const },
        ],
        failure: null,
      },
      enabled: true,
      models_dir: null,
      device: null,
    };
    expect(card(ready)?.tone).toBe('ready');
    expect(card(ready)?.detail).toBe('loaded, loaded');

    const stopped = {
      snapshot: {
        models: [{ role: 'transcribing', file: 'a.gguf', state: 'pending' as const }],
        failure: { cancelled: { kept: 1, total: 2 } },
      },
      enabled: true,
      models_dir: null,
      device: null,
    };
    expect(card(stopped)?.tone).toBe('bad');
    expect(card(stopped)?.detail).toBe('stopped');
  });
});

describe('the band', () => {
  /**
   * Every decision on the strip: a bind error is bad and names the port it
   * failed on, a listener still coming up is a warning, an account that
   * bailed is bad, and the accounts card counts ready separately from
   * bailed. A stub replacing any of these three cards wholesale is green
   * without this.
   */
  const card = (wire: HomeWire, title: string) =>
    homeView(wire, '127.0.0.1:8790').band.find((entry) => entry.title === title);

  it('reads the gateway listener', () => {
    const gateway = (gateway: HomeWire['accounts']['gateway']) =>
      card({ ...homeWire, accounts: { ...homeWire.accounts, gateway } }, 'gateway');
    expect(gateway({ ready: true, port: 8787, bind_error: null })).toEqual({
      title: 'gateway',
      tone: 'ready',
      value: 'bound :8787',
      detail: 'inference listener',
      href: null,
    });
    expect(gateway({ ready: false, port: 8787, bind_error: null })?.tone).toBe('warn');
    expect(gateway({ ready: true, port: 9, bind_error: 'address in use' })).toEqual({
      title: 'gateway',
      tone: 'bad',
      value: 'failed :9',
      detail: 'address in use',
      href: null,
    });
  });

  it('reads the account pool', () => {
    const accounts = (loading: HomeWire['accounts']['loading'], all_loaded: boolean) =>
      card({ ...homeWire, accounts: { ...homeWire.accounts, loading, all_loaded } }, 'accounts');
    const row = (state: HomeWire['accounts']['loading'][number]['state']) => ({
      display_name: 'Acct',
      state,
      last_error: null,
      retry_after: null,
      auth: 'token',
    });

    expect(accounts([row('ready'), row('ready')], true)).toEqual({
      title: 'accounts',
      tone: 'ready',
      value: '2 ready',
      detail: 'probed',
      href: null,
    });
    expect(accounts([row('loading')], false)?.tone, 'still probing').toBe('warn');
    expect(accounts([row('ready'), row('bailed')], true)).toEqual({
      title: 'accounts',
      tone: 'bad',
      value: '1 ready \u{b7} 1 bailed',
      detail: 'probed',
      href: null,
    });
  });

  /**
   * The dictation card is the way into the models page, and it is the only
   * card that opens anything: the band is where a reader looks for
   * dictation, and that page is what draws it.
   */
  it('opens the models page from the dictation card alone', () => {
    const cards = homeView(homeWire, '').band;

    expect(cards.map((entry) => entry.title)).toEqual(['gateway', 'web', 'dictation', 'accounts']);
    expect(cards.map((entry) => entry.href)).toEqual([null, null, '/models', null]);
  });

  it('reads dictation as off when the config turned it off', () => {
    expect(
      card(
        {
          ...homeWire,
          dictate: {
            snapshot: { models: [], failure: null },
            enabled: false,
            models_dir: null,
          },
        },
        'dictation',
      ),
    ).toEqual({
      title: 'dictation',
      tone: 'off',
      value: 'off',
      detail: 'enabled = false',
      // The door stays: /models draws the off state and names the key that
      // would switch it on, which is where a reader who is looking for
      // dictation ends up.
      href: '/models',
    });
  });
});

describe('what a row says', () => {
  it('reads a task status as a chip', () => {
    expect(chipFor('in_progress')).toBe('in progress');
    expect(chipFor('completed')).toBe('done');
    expect(chipFor('pending')).toBe('pending');
    // The status a wait is spelled with now, which the fallback words for the
    // strip: the vocabulary moved to `waiting` when it left `blocked` behind.
    expect(chipFor('waiting')).toBe('waiting');
  });

  /**
   * Every one of `homeFrom`'s narrow arms, because each is a value this
   * client is older than turned into a known one where it enters.
   *
   * The lifecycle arm keeps `markOf`'s switch exhaustive. The model-state
   * arm is the one that would otherwise reach a renderer that reads a
   * property off it: an unknown string there is not a card drawn wrongly,
   * it is a page that does not draw.
   */
  it('narrows a lifecycle it does not know as the snapshot is read', () => {
    const unknown = { ...FLEET, agents: [{ ...FLEET_AGENT, lifecycle: 'Resting' as never }] };
    const lead = rowsOf(homeFrom(unknown)).lead;
    expect(lead?.state).toEqual({ kind: 'lifecycle', lifecycle: 'Idle' });
  });

  it("narrows a seat's tree gate it does not know", () => {
    const unknown: HomeWire = {
      ...homeWire,
      agents: [{ ...AGENT, work: { branch: 'main', changed: 1, gate: 'unreadable' as never } }],
    };
    // The FIELD, not a rendering of it: `gateLine` switches over the four this
    // client knows, and a fifth would fall out of the switch rather than draw
    // a wrong line - which is a row whose `what` cell is silently empty.
    expect(
      homeFrom(unknown).agents[0]?.work?.gate,
      'a gate this client is older than reached the row',
    ).toBe('in_repo');
  });

  it('narrows a pending kind it does not know', () => {
    const unknown = { ...FLEET, agents: [{ ...FLEET_AGENT, pending: 'elicit' as never }] };
    expect(rowsOf(homeFrom(unknown)).lead.pending).toBe('permission');
  });

  /**
   * The one step back, which is a SHAPE and not only a name.
   *
   * A v7 forge sent each of a project's entries as the TASK alone, flat, under
   * `tasks` - no `worked_secs`, no marks, an `artifact` rather than `links`,
   * and an `estimate` that is its words. An entry like that is not a row, so a
   * read that only renamed the key takes `entry.task.status` off `undefined`
   * and THROWS while drawing: nothing renders and the skew notice is the only
   * thing on the page. What that server never stated reads as the neutral
   * value instead.
   */
  it('reads a project whose rows crossed as flat v7 tasks', () => {
    const first = homeWire.projects[0];
    if (first === undefined) throw new Error('the fixture holds no project');
    const stepBack = {
      ...homeWire,
      projects: [
        {
          ...first,
          rows: undefined,
          // Shaped by the v7 server's own wire type (`ProjectWire.tasks` was
          // `Vec<Task>`), not by wrapping a v8 row under the old key.
          tasks: [
            {
              id: 'v7-1',
              project_name: 'proj',
              subject: 'a row from a v7 server',
              active_form: null,
              detail: null,
              status: 'in_progress',
              owner: null,
              parent: null,
              artifact: 'https://example.test/pull/7',
              estimate: '1d',
              created_at: { secs_since_epoch: 1_700_000_000, nanos_since_epoch: 0 },
              updated_at: { secs_since_epoch: 1_700_000_000, nanos_since_epoch: 0 },
            },
          ],
        },
      ],
    } as unknown as HomeWire;

    const rows = homeFrom(stepBack).projects[0]?.rows ?? [];
    expect(rows, 'the flat task did not read as a row').toHaveLength(1);
    const task = rows[0]?.task;
    expect(task?.subject, 'the task itself was lost').toBe('a row from a v7 server');
    expect(task?.status, 'the status did not come off the entry').toBe('in_progress');
    // The fields v7 never sent read as what this client draws for nothing,
    // rather than as `undefined` reaching the board's arithmetic.
    expect(rows[0]?.worked_secs).toBe(0);
    expect(rows[0]?.marks.overdue).toBe(false);
    expect(rows[0]?.rollup).toBeNull();
    expect(task?.rank).toBeNull();
    expect(task?.waiting_on).toBeNull();
    expect(task?.attempt).toBe(0);
    // `artifact` is `links` now: it crosses as one rather than being dropped.
    expect(
      task?.links.map((link) => link.target),
      'the v7 artifact did not cross',
    ).toEqual(['https://example.test/pull/7']);
    // And the estimate keeps its words with no seconds to be measured against,
    // which the board reads as no measure rather than as a NaN ratio.
    expect(task?.estimate).toEqual({ words: '1d', secs: 0 });
  });

  it('reads a project a server sends no rows for as an empty board', () => {
    const first = homeWire.projects[0];
    if (first === undefined) throw new Error('the fixture holds no project');
    const neither = {
      ...homeWire,
      projects: [{ ...first, rows: undefined, tasks: undefined }],
    } as unknown as HomeWire;
    expect(homeFrom(neither).projects[0]?.rows).toEqual([]);
  });

  it('narrows an account state it does not know', () => {
    const unknown: HomeWire = {
      ...homeWire,
      accounts: { ...homeWire.accounts, loading: [{ ...LOADING, state: 'renewing' as never }] },
    };
    // The FIELD, not a rendering of it. `band()` tells `ready` from `bailed`
    // and nothing else, so an unknown state draws the identical card to the
    // narrowed one and a card assertion cannot see this arm at all.
    expect(homeFrom(unknown).accounts.loading[0]?.state).toBe('loading');
  });

  it('narrows a dictation state it does not know rather than crashing on it', () => {
    const unknown: HomeWire = {
      ...homeWire,
      dictate: {
        ...homeWire.dictate,
        snapshot: {
          models: [
            // The state is deliberately outside the union, which is the whole
            // test, so the cast is the instrument rather than a shortcut.
            { role: 'transcribing', file: 'a.gguf', state: 'warming' } as unknown as DictateModel,
          ],
          failure: null,
        },
      },
    };
    const card = homeView(homeFrom(unknown), '').band.find((entry) => entry.title === 'dictation');
    expect(card?.detail).toBe('waiting');
    expect(card?.value).toBe('0 of 1 loaded');
  });
});

describe('the artifact a row shows', () => {
  it('reads as one short token', () => {
    expect(artifactLabel('https://github.com/o/r/pull/148')).toBe('PR 148');
    expect(artifactLabel('https://github.com/o/r/issues/12')).toBe('#12');
    expect(artifactLabel('/tmp/work/output/report.md')).toBe('report.md');
    expect(artifactLabel('https://example.com/a/b/')).toBe('b');
  });

  it('is followable only as a whole absolute URL', () => {
    expect(followable('https://github.com/o/r/pull/148')).toBe('https://github.com/o/r/pull/148');
    expect(followable(' /tmp/notes ')).toBeNull();
    expect(followable('see https://example.com now')).toBeNull();
    expect(followable('git@github.com:o/r.git')).toBeNull();
  });
});

describe('how long ago a row last wrote', () => {
  const at = (seconds: number) => ({ secs_since_epoch: seconds, nanos_since_epoch: 0 });

  it('rolls over at a minute, an hour and a day', () => {
    const now = 100 * 86400 * 1000;
    expect(elapsedLabel(at(now / 1000), now)).toBe('now');
    expect(elapsedLabel(at(now / 1000 - 59), now)).toBe('now');
    expect(elapsedLabel(at(now / 1000 - 60), now)).toBe('1m');
    expect(elapsedLabel(at(now / 1000 - 3600), now)).toBe('1h');
    expect(elapsedLabel(at(now / 1000 - 86400), now)).toBe('1d');
  });

  it('reads a live session with no transcript as now rather than an absence', () => {
    const row: Row = { ...LEAD_ROW, lastActivity: null };
    expect(whenOf(row, Date.now())).toBe('now');
  });
});

describe('the fatal on the wire', () => {
  it('keeps the words, and reads anything else as no fatal', () => {
    expect(homeFrom({ ...homeWire, fatal_error: 'stopped hard' }).fatal_error).toBe('stopped hard');
    expect(
      homeFrom({ ...homeWire, fatal_error: 7 as unknown as string }).fatal_error,
      'a value that is not words reads as no fatal',
    ).toBeNull();
  });
});
