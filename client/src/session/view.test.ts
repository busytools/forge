import { describe, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import session from '../dev/fixtures/session.json';
import type { AgentRow, HomeWire, ProjectWire } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import {
  accountChip,
  fleetCount,
  gitSection,
  gotifySection,
  headerFacts,
  mcpState,
  memoryLabel,
  monitorLabel,
  processHeadline,
  processTree,
  type ProcessNode,
  railGroups,
  railMark,
  schedulesSection,
  seatState,
  slackSection,
  tasksSection,
  untilOf,
} from './view';
import { sessionFrom, type ProcessSnapshot, type SessionRecord } from './wire';

const record: SessionRecord = sessionFrom(session);

/** The fixture's one project, which every home-shaped test starts from. */
function project(): ProjectWire {
  const first = homeWire.projects[0];
  if (first === undefined) throw new Error('the fixture holds no project');
  return first;
}

/** The fixture's lead, which every rail test starts from. */
function lead(): AgentRow {
  const first = homeWire.agents[0];
  if (first === undefined) throw new Error('the fixture holds no agent');
  return first;
}

const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

const withHome = (change: Partial<HomeWire>): HomeWire => ({ ...homeWire, ...change });
const withProject = (change: Partial<ProjectWire>): HomeWire =>
  withHome({ projects: [{ ...project(), ...change }] });

describe('the boundary', () => {
  /**
   * **A value outside the shipped set is narrowed where it enters, and the
   * fallback is the least-alarming member rather than the true one.** The
   * fixture only ever holds values inside the set, so nothing else exercises
   * the arm - and it is the one thing that keeps a client older than its
   * server from drawing a state it cannot name.
   */
  it('turns a state this client is older than into one it knows', () => {
    const ahead = sessionFrom({
      ...(session as unknown as Record<string, unknown>),
      monitors: [
        {
          tool_use_id: 'm1',
          task_id: null,
          description: 'a monitor',
          command: 'ls',
          persistent: false,
          timeout_ms: 0,
          status: 'quantum',
          output_file: null,
          ended_at: null,
        },
      ],
      header: { ...record.header, effort: 'extreme', permission_mode: 'sentient' },
    });
    expect(ahead.monitors[0]?.status, 'a status from the future claimed the work is over').toBe(
      'running',
    );
    expect(ahead.header.effort).toBe('medium');
    expect(ahead.header.permission_mode).toBe('default');
  });
});

describe('the header facts', () => {
  it('names a model the CLI gave no long name for by its resolved id', () => {
    const facts = headerFacts({
      ...record.header,
      model: { resolved_id: 'claude-opus-5-5', display_name_long: '' },
    });
    expect(facts.model, 'an unnamed model drew an empty cell').toBe('claude-opus-5-5');
  });

  it('draws a dash rather than dropping a fact the session has not stated', () => {
    const facts = headerFacts({
      ...record.header,
      model: null,
      context: { percent: null, max_tokens: null },
    });
    expect(facts.model).toBe('\u{2014}');
    expect(facts.mode, 'a mode nothing has reported claimed a value').toBeNull();
    expect(facts.percent).toBeNull();
  });

  it('carries the mode the session runs in, with the class its colour reads', () => {
    const facts = headerFacts({ ...record.header, permission_mode: 'bypassPermissions' });
    expect(facts.mode).toEqual({ wire: 'bypassPermissions', klass: 'bypass' });
  });
});

describe('the rail', () => {
  it('groups a project by its strongest row, not by its lead alone', () => {
    const worker: AgentRow = {
      ...lead(),
      slot: { ...lead().slot, label: 'held-worker' },
      label: 'held-worker',
      lifecycle: 'Idle',
      pending: null,
      reason: null,
    };
    const held: AgentRow = { ...worker, pending: 'question' };
    const groups = railGroups(withHome({ agents: [{ ...lead(), pending: null }, held] }), LEAD, 0);
    const needs = groups.find((group) => group.heading === 'needs you');
    expect(
      needs?.projects[0]?.why?.line,
      'a held worker did not lift its project out of working',
    ).toBe('asked you a question');
  });

  it('marks the seat the page is showing', () => {
    const groups = railGroups(homeWire, LEAD, 0);
    const shown = groups.flatMap((group) => group.projects).find((entry) => entry.current);
    expect(shown?.name).toBe('proj');
  });

  /**
   * The rail's mark is its own mapping: the home's row draws a class on the
   * row and a dot shape inside it, and this puts the state's name on a bare
   * dot. Sign-in needed is a failure a person has to act on, and neither
   * surface has an auth shape of its own.
   */
  it('draws the same mark for a failed seat and one needing sign-in', () => {
    expect(railMark({ kind: 'lifecycle', lifecycle: 'AuthRequired' })).toBe('failed');
    expect(railMark({ kind: 'lifecycle', lifecycle: 'Failed' })).toBe('failed');
    expect(railMark({ kind: 'lifecycle', lifecycle: 'Sleeping' })).toBe('off');
    expect(railMark({ kind: 'never-started' })).toBe('off');
    expect(railMark({ kind: 'unseen' })).toBe('unseen');
  });

  it('counts the fleet by its seats, not by the rows a group drew', () => {
    expect(fleetCount(homeWire)).toBe('1 live / 1');
    expect(fleetCount(withHome({ projects: [] }))).toBe('0 live / 0');
  });
});

describe('the seat', () => {
  it('reads a seat the roster does not name as one nothing has started', () => {
    const state = seatState(homeWire, { org: 'TestOrg', project: 'proj', label: 'nobody' });
    expect(state.waking, 'a seat with no row claimed a session').toBe(true);
    expect(state.mark).toBe('off');
    expect(state.name).toBe('nobody');
  });

  it('names a lead for its project and a worker for its label', () => {
    expect(seatState(homeWire, LEAD).name).toBe('proj');
    expect(seatState(homeWire, { ...LEAD, label: 'w1' }).name).toBe('w1');
  });
});

describe('the git section', () => {
  it('leads with the branch and the count the record carries', () => {
    const git = gitSection({
      ...record,
      work: { branch: 'web-home-layout', changed: 8, gate: 'in_repo' },
      pr: { number: 1203, url: 'https://example.test/pull/1203' },
      closes: [{ number: 1200, url: 'https://example.test/issues/1200' }],
    });
    expect(git?.summary).toBe('web-home-layout \u{b7} 8 files');
    expect(git?.pr).toBe(1203);
    expect(git?.closes).toBe('#1200');
  });

  /**
   * A seat outside a repository has no branch and no count, and the gate line
   * is then the whole of what the section says. Drawing nothing would read as
   * a seat with nothing to report rather than as a tree that could not be
   * read.
   */
  it('draws the reason a tree could not be read', () => {
    const git = gitSection({
      ...record,
      work: { branch: null, changed: null, gate: 'gone' },
      pr: null,
      closes: [],
    });
    expect(git?.gate).toBe('its working directory is not there');
    expect(git?.summary).toBe('');
  });

  /**
   * The section is drawn for every seat with a tree, and OPENS only when there
   * is something under it: a clean tree on no pull request would otherwise
   * lead the inspector with an open section and nothing in it.
   */
  it('opens the section only when there is a body to open on', () => {
    expect(
      gitSection({
        ...record,
        work: { branch: 'main', changed: 0, gate: 'in_repo' },
        pr: null,
        closes: [],
      }).open,
      'a clean tree with nothing to show opened its section',
    ).toBe(false);
  });

  it('draws the section closed, with its branch, for a tree nothing moved in', () => {
    const git = gitSection({
      ...record,
      work: { branch: 'main', changed: 0, gate: 'in_repo' },
      pr: null,
      closes: [],
    });
    expect(git.summary).toBe('main');
    expect(git.pr).toBeNull();
  });
});

describe('the inbox sections', () => {
  it('says which of the two an empty MCP read is', () => {
    expect(mcpState({ name: 'forge', status: 'failed', error: '  ' })).toBe('failed');
    expect(mcpState({ name: 'forge', status: 'failed', error: ' the CLI refused ' })).toBe(
      'the CLI refused',
    );
    expect(mcpState({ name: 'forge', status: 'connected', tools: [] })).toBe('no tools');
    expect(mcpState({ name: 'forge', status: 'connected', tools: [{}, {}] })).toBe('2 tools');
  });

  it('takes the floor of an unbounded subscription off the whole set', () => {
    const unbounded = withHome({
      connectors: {
        gotify: {
          connected: true,
          subscriptions: [
            { applications: ['homelab', 'alerts'], min_priority: 4 },
            { applications: ['homelab'], min_priority: null },
          ],
        },
        slack: { connected_workspaces: [], load_failed: false, subscriptions: [] },
      },
    });
    expect(gotifySection(unbounded)?.rows[1]?.v, 'a set with no floor claimed one').toBe('any');
    expect(gotifySection(unbounded)?.rows[0]?.v).toBe('homelab, alerts');
  });

  /**
   * A subscription watches a workspace, and the section says so by nesting:
   * the terminal put two `&nbsp;` in front of the target, which is a space
   * standing in for a level of hierarchy.
   *
   * **The two class targets are strings here because that is what the socket
   * sends.** `SlackSubscriptionTarget` is an externally tagged enum, so
   * `Mentions` and `DirectMessages` cross as bare strings and only
   * `Conversation` is an object.
   */
  it('hangs each slack subscription off the workspace it watches', () => {
    const slack = slackSection(
      withHome({
        connectors: {
          gotify: { connected: false, subscriptions: [] },
          slack: {
            connected_workspaces: [['Trust Machines', true]],
            load_failed: false,
            subscriptions: [
              { id: 's1', workspace: 'Acme', target: 'Mentions' },
              {
                id: 's2',
                workspace: 'Trust Machines',
                target: { Conversation: { id: 'C1', name: '#alerts', mode: 'All' } },
              },
            ],
          },
        },
      }),
    );
    expect(slack?.summary).toBe('2 workspaces');
    expect(slack?.workspaces.find((entry) => entry.name === 'Trust Machines')?.subs).toEqual([
      { id: 's2', k: '#alerts', v: 'every message' },
    ]);
    // A workspace the pump has not reported still draws, with its own state.
    expect(slack?.workspaces.find((entry) => entry.name === 'Acme')).toEqual({
      name: 'Acme',
      connected: false,
      subs: [{ id: 's1', k: 'mentions anywhere', v: 'mentions only' }],
    });
  });
});

describe('the schedules section', () => {
  it('reads a time already past as due rather than counting into the past', () => {
    expect(untilOf({ secs_since_epoch: 0 }, 1_700_000_000_000)).toBe('in a minute');
    expect(untilOf({ secs_since_epoch: 1_700_003_600 }, 1_700_000_000_000)).toBe('in 1h');
    expect(untilOf(null, 0)).toBe('due now');
  });

  it('names a schedule by its description, else its prompt', () => {
    const now = 1_700_000_000_000;
    const view = schedulesSection(
      [
        {
          id: 'c1',
          project_name: 'proj',
          kind: { Recurring: '0 9 * * *' },
          prompt: 'sweep the deps\nand more',
          description: 'deps sweep',
          created_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
          next_fire: { secs_since_epoch: 1_700_003_600, nanos_since_epoch: 0 },
        },
        {
          id: 'c2',
          project_name: 'proj',
          kind: { Once: { secs_since_epoch: 0, nanos_since_epoch: 0 } },
          prompt: 'plugin audit\nand more',
          created_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
          next_fire: { secs_since_epoch: 1_700_003_600, nanos_since_epoch: 0 },
        },
      ],
      now,
    );
    expect(view.rows.map((row) => row.k)).toEqual(['deps sweep', 'plugin audit']);
    expect(view.rows[1]?.v).toBe('in 1h \u{b7} one-shot');
    expect(view.summary).toBe('2');
  });
});

describe('the tasks section', () => {
  it('reads in-progress first and counts what is done', () => {
    const view = tasksSection([
      {
        id: 't1',
        project_name: 'proj',
        subject: 'done already',
        active_form: null,
        detail: null,
        status: 'completed',
        owner: null,
        parent: null,
        artifact: null,
        estimate: null,
        created_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
        updated_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
      },
      {
        id: 't2',
        project_name: 'proj',
        subject: 'still going',
        active_form: null,
        detail: null,
        status: 'in_progress',
        owner: LEAD,
        parent: null,
        artifact: 'https://example.test/pull/1204',
        estimate: '2h',
        created_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
        updated_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
      },
    ]);
    expect(view.summary).toBe('1 of 2');
    expect(view.rows[0]?.subject).toBe('still going');
    expect(view.rows[0]?.klass).toBe('tk now');
    expect(view.rows[0]?.meta).toBe('in progress \u{b7} PR 1204 \u{b7} 2h');
    expect(view.rows[1]?.klass).toBe('tk done');
  });
});

/** Every pid in a tree, wherever it sits in it. */
function flatten(nodes: ProcessNode[]): number[] {
  return nodes.flatMap((node) => [node.pid, ...flatten(node.children)]);
}

describe('the processes section', () => {
  const walk: ProcessSnapshot = {
    scanned_at: { secs_since_epoch: 1_700_000_000, nanos_since_epoch: 0 },
    processes: [
      { pid: 10, parent_pid: 1, name: 'claude', command: 'claude', memory_bytes: 0 },
      {
        pid: 20,
        parent_pid: 10,
        name: 'cargo',
        command: '/opt/homebrew/bin/cargo nextest run',
        memory_bytes: 412,
      },
      { pid: 30, parent_pid: 20, name: 'rustc', command: 'rustc', memory_bytes: 0 },
      { pid: 40, parent_pid: 999, name: 'orphan', command: 'orphan --flag', memory_bytes: 0 },
    ],
  };

  it('nests a child under its parent wherever the walk put it', () => {
    const roots = processTree(walk);
    const claude = roots.find((node) => node.pid === 10);
    expect(claude?.children.map((node) => node.pid)).toEqual([20]);
    expect(claude?.children[0]?.children.map((node) => node.pid)).toEqual([30]);
  });

  it('makes a row whose parent the walk did not carry a root of its own', () => {
    expect(processTree(walk).some((node) => node.pid === 40)).toBe(true);
  });

  /**
   * A pid cycle reaches no root, so the second pass is what draws it - every
   * row once, and nothing hangs.
   */
  it('draws every row once when the walk holds a cycle', () => {
    const cycle = processTree({
      scanned_at: walk.scanned_at,
      processes: [
        { pid: 1, parent_pid: 2, name: 'a', command: 'a', memory_bytes: 0 },
        { pid: 2, parent_pid: 1, name: 'b', command: 'b', memory_bytes: 0 },
      ],
    });
    expect(flatten(cycle).sort()).toEqual([1, 2]);
  });

  it('strips the path off the executable and unwraps a shell wrapper', () => {
    expect(processHeadline(walk.processes[1] as never)).toBe('cargo nextest run');
    expect(
      processHeadline({
        pid: 1,
        parent_pid: 0,
        name: 'zsh',
        command: `zsh -c "eval 'gh run watch 12' < /dev/null"`,
        memory_bytes: 0,
      }),
    ).toBe('gh run watch 12');
  });

  it('reads memory in the unit the reader thinks in', () => {
    expect(memoryLabel(0)).toBe('0 B');
    expect(memoryLabel(1024)).toBe('1 KB');
    expect(memoryLabel(1024 * 1024)).toBe('1 MB');
    expect(memoryLabel(1024 * 1024 * 1024)).toBe('1.0 GB');
  });
});

describe('the monitors section', () => {
  it('draws an age only when the record stated an instant', () => {
    const base = {
      tool_use_id: 'm1',
      task_id: null,
      description: 'ci-watch',
      command: 'gh run watch',
      persistent: false,
      timeout_ms: 0,
      output_file: null,
    };
    expect(monitorLabel({ ...base, status: 'running', ended_at: null }, 0)).toBe('running');
    expect(monitorLabel({ ...base, status: 'completed', ended_at: null }, 0)).toBe('completed');
    expect(
      monitorLabel(
        {
          ...base,
          status: 'completed',
          ended_at: { secs_since_epoch: 1_699_999_880, nanos_since_epoch: 0 },
        },
        1_700_000_000_000,
      ),
    ).toBe('completed 2m');
  });
});

describe('the account chip', () => {
  it('draws nothing for a project that chips no account', () => {
    expect(accountChip(homeWire, LEAD)).toBeNull();
    expect(accountChip(homeWire, { ...LEAD, project: 'nowhere' })).toBeNull();
  });

  it('reads the pool own words for the account the project would bind to', () => {
    const home = withProject({ chip: { account_name: 'Acct', state: 'loading' } });
    const view = accountChip(home, LEAD);
    expect(view?.state).toBe('probing');
    expect(view?.tone).toBe('wait');
    expect(view?.auth).toBe('token');
    expect(view?.windows).toEqual([]);
  });

  /**
   * A window the account is past reports above 100, and the bar stops at full
   * while the label does not: an account over its cap is the one case the row
   * has to be readable in, and a label that echoed the bar would hide it.
   */
  it('states the figure a past-cap window reports rather than the bar width', () => {
    const home = withProject({ chip: { account_name: 'Acct', state: 'ready' } });
    const over = {
      ...home,
      accounts: {
        ...home.accounts,
        usage: [
          {
            display_name: 'Acct',
            snapshot: { five_hour: { utilization: 101, reset_description: 'in 2h' } },
          },
        ],
      },
    };
    expect(accountChip(over, LEAD)?.windows[0]).toEqual({
      label: '5h',
      percent: 100,
      text: '101%',
      reset: 'in 2h',
    });
  });

  /**
   * What the poller knows beyond the windows: per-key spend and the account's
   * remaining credit. Both are `Option` on the wire, so a pool that reports
   * neither draws no rows rather than zeroes.
   */
  it('draws the spend and the balance the poller reported, and neither when it did not', () => {
    const home = withProject({ chip: { account_name: 'Acct', state: 'ready' } });
    const billed = {
      ...home,
      accounts: {
        ...home.accounts,
        usage: [
          {
            display_name: 'Acct',
            snapshot: {
              spend: { daily: 1.5, weekly: 10, monthly: 42 },
              balance: 12.25,
            },
          },
        ],
      },
    };
    expect(accountChip(billed, LEAD)?.spend).toEqual({
      daily: '$1.50',
      weekly: '$10.00',
      monthly: '$42.00',
    });
    expect(accountChip(billed, LEAD)?.balance).toBe('$12.25');
    expect(accountChip(home, LEAD)?.spend, 'an absent spend drew as zeroes').toBeNull();
    expect(accountChip(home, LEAD)?.balance).toBeNull();
  });
});
