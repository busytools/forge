import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import type {
  AgentRow,
  DictateModel,
  HomeWire,
  Lifecycle,
  ProjectWire,
  Task,
  TaskStatus,
  WireTime,
} from '../wire/home';
import { homeFrom } from '../wire/home';
import {
  artifactLabel,
  availableVersion,
  chipFor,
  countsOf,
  elapsedLabel,
  gateLine,
  failureFile,
  failureKind,
  followable,
  homeView,
  markOf,
  modelState,
  refusal,
  waitingOn,
  whenOf,
  type HomeView,
  type OrgSection,
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
};

/** One project row: the project, and the per-row reads the home draws it from. */
function project(org: string, name: string, over: Partial<ProjectWire> = {}): ProjectWire {
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
    tasks: [],
    crons: [],
    would_bind: true,
    chip: null,
    ...over,
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
    artifact: null,
    estimate: null,
    created_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
    updated_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
  };
}

function agent(org: string, project: string, label: string, lifecycle: Lifecycle): AgentRow {
  return {
    slot: { org, project, label },
    label,
    lifecycle,
    has_background_work: false,
    pending: null,
    pending_depth: 0,
    last_activity: null,
    reason: null,
    peer: { outgoing: 0, incoming: 0, delivery_failed: 0 },
    peer_failure_at: null,
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

/** The org of that name, which the fixtures below all have. */
function orgNamed(view: HomeView, name: string): OrgSection {
  const org = view.orgs.find((section) => section.name === name);
  if (org === undefined) throw new Error(`${name} is not in this view`);
  return org;
}

/** The first row of the first project, which every fixture here draws. */
function leadOf(view: HomeView): Row {
  const entry = first(view.orgs, 'org').projects[0];
  if (entry === undefined) throw new Error('the first org carries no project');
  return entry.lead;
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
  task: null,
  pending: null,
  reason: null,
  lastActivity: null,
};

describe('the fleet the snapshot describes', () => {
  /**
   * One fixture is one project with one agent, so a second org, a second
   * project in an org and any worker row are unreachable. Three separate
   * readings go wrong on a real fleet and none of them on the fixture: an
   * org's live count, the name a worker row carries, and the header's two
   * numbers swapped.
   */
  it('puts each project under its own org and counts what is live', () => {
    const view = homeView(FLEET, '');
    expect(view.orgs.map((org) => org.name)).toEqual(['Busytools', 'Personal']);
    expect(countsOf(orgNamed(view, 'Busytools'))).toBe('2 live');
    expect(countsOf(orgNamed(view, 'Personal'))).toBe('1 live');
    expect(orgNamed(view, 'Busytools').projects.map((entry) => entry.lead.name)).toEqual([
      'forge',
      'notes',
    ]);
  });

  it('names a worker row for the worker and only the lead for its project', () => {
    const forge = homeView(FLEET, '').orgs[0]?.projects[0];
    expect(forge?.lead.name).toBe('forge');
    expect(forge?.workers.map((worker) => worker.name)).toEqual(['w1']);
    expect(forge?.workers[0]?.slot.label).toBe('w1');
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

  it('says what it counts for every shape of org', () => {
    const busytools = (wire: HomeWire) => orgNamed(homeView(wire, ''), 'Busytools');
    // Every agent of one project gone, which leaves that project dormant and
    // its sibling live. Dropping only the lead would not: a worker row still
    // means somebody started it.
    const withoutForge: HomeWire = {
      ...FLEET,
      agents: FLEET.agents.filter((row) => row.slot.project !== 'forge'),
    };

    expect(countsOf(busytools({ ...FLEET, agents: [] })), 'nothing live').toBe('2 asleep');
    expect(countsOf(busytools(FLEET)), 'nothing asleep').toBe('2 live');
    expect(countsOf(busytools(withoutForge)), 'both').toBe('1 live \u{b7} 1 asleep');
  });
});

describe('a row over the fleet', () => {
  it('groups a project under its org and names the lead row for the project', () => {
    const view = homeView(homeWire, '127.0.0.1:8790');
    const org = orgNamed(view, 'TestOrg');
    expect(org.projects).toHaveLength(1);
    // The lead's label is its identity, not what the row is called here.
    expect(leadOf(view).name).toBe('proj');
    expect(leadOf(view).slot.label).toBe('lead');
    expect(countsOf(org)).toBe('1 live');
  });

  it('reads the fixture the server pinned, without re-deriving any state', () => {
    const view = homeView(homeWire, 'ws://127.0.0.1:8790/socket');
    const lead = view.orgs[0]?.projects[0]?.lead;
    expect(lead?.state).toEqual({ kind: 'lifecycle', lifecycle: 'Idle' });
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

    const lead = leadOf(homeView(withSessions([]), ''));
    expect(lead.state).toEqual({ kind: 'never-started' });
    expect(whenOf(lead, Date.now())).toBe('never');

    // The same project with a session behind it is asleep instead: a forge
    // restart leaves every project in that case, so reading the lifecycle
    // alone would call the whole fleet new.
    expect(
      homeView(withSessions([{ last_activity: { secs_since_epoch: 0, nanos_since_epoch: 0 } }]), '')
        .orgs[0]?.projects[0]?.lead.state,
    ).toEqual({ kind: 'lifecycle', lifecycle: 'Sleeping' });
  });

  it('promotes a backgrounded task to running, and leaves idle alone without one', () => {
    const wire = (has_background_work: boolean): HomeWire => ({
      ...homeWire,
      agents: [{ ...AGENT, has_background_work }],
    });
    expect(homeView(wire(true), '').orgs[0]?.projects[0]?.lead.state).toEqual({
      kind: 'lifecycle',
      lifecycle: 'Running',
    });
    expect(homeView(wire(false), '').orgs[0]?.projects[0]?.lead.state).toEqual({
      kind: 'lifecycle',
      lifecycle: 'Idle',
    });
  });

  it('draws an empty fleet as the empty state rather than a broken page', () => {
    expect(homeView({ ...homeWire, projects: [], agents: [] }, '').orgs).toEqual([]);
  });

  it('names the two refusals, and neither when a spawn would run', () => {
    expect(refusal(false, true)).toBe('no model declared - add `model` to this project');
    expect(refusal(true, false)).toBe('no usable accounts');
    expect(refusal(true, true)).toBeNull();
  });
});

describe('the cells the reshape made drawable', () => {
  /** One project of the fixture fleet, with its row's reads varied. */
  const withRow = (over: Partial<ProjectWire>): HomeWire => ({
    ...homeWire,
    projects: [{ ...PROJECT, ...over }],
  });

  /**
   * A count of zero is not a fact about the tree, so it draws nothing. A
   * `changed` of 0 rendered as `0 files` would say a project has moved
   * nothing, which is what the cell already means when it is empty.
   */
  it('draws the branch and the count, and nothing for a count of zero', () => {
    const place = (changed: number | null) =>
      leadOf(homeView(withRow({ work: { branch: 'main', changed, gate: 'in_repo' } }), '')).place;
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
      homeView(withRow({ tasks: [task('completed', 'shipped'), task('in_progress', 'now')] }), ''),
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
    const marked: HomeWire = { ...homeWire, projects: [idle], agents: [AGENT], unseen: [slot] };

    expect(leadOf(homeView(marked, '')).state).toEqual({ kind: 'unseen' });
    // The same seat with nothing unseen is idle, so the mark is the list
    // rather than the lifecycle.
    expect(leadOf(homeView({ ...marked, unseen: [] }, '')).state).toEqual({
      kind: 'lifecycle',
      lifecycle: 'Idle',
    });
  });

  /**
   * A project whose tree is not there says so rather than showing a blank
   * `where` cell that reads as "no branch".
   */
  it('says why a row has no branch to show', () => {
    const row = leadOf(
      homeView(withRow({ work: { branch: null, changed: null, gate: 'gone' } }), ''),
    );
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
    });
    expect(gateway({ ready: false, port: 8787, bind_error: null })?.tone).toBe('warn');
    expect(gateway({ ready: true, port: 9, bind_error: 'address in use' })).toEqual({
      title: 'gateway',
      tone: 'bad',
      value: 'failed :9',
      detail: 'address in use',
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
    });
    expect(accounts([row('loading')], false)?.tone, 'still probing').toBe('warn');
    expect(accounts([row('ready'), row('bailed')], true)).toEqual({
      title: 'accounts',
      tone: 'bad',
      value: '1 ready \u{b7} 1 bailed',
      detail: 'probed',
    });
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
            device: null,
          },
        },
        'dictation',
      ),
    ).toEqual({ title: 'dictation', tone: 'off', value: 'off', detail: 'enabled = false' });
  });
});

describe('what a row says', () => {
  it('names the two asks differently', () => {
    expect(waitingOn('question')).toBe('asked you a question');
    expect(waitingOn('permission')).toBe('a permission prompt is waiting');
  });

  it('reads a task status as a chip', () => {
    expect(chipFor('in_progress')).toBe('in progress');
    expect(chipFor('completed')).toBe('done');
    expect(chipFor('pending')).toBe('pending');
    expect(chipFor('blocked')).toBe('blocked');
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
    const lead = homeView(homeFrom(unknown), '').orgs[0]?.projects[0]?.lead;
    expect(lead?.state).toEqual({ kind: 'lifecycle', lifecycle: 'Idle' });
  });

  it('narrows a pending kind it does not know', () => {
    const unknown = { ...FLEET, agents: [{ ...FLEET_AGENT, pending: 'elicit' as never }] };
    expect(homeView(homeFrom(unknown), '').orgs[0]?.projects[0]?.lead.pending).toBe('permission');
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
