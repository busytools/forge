import { describe, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import session from '../dev/fixtures/session.json';
import { PROTOCOL_VERSION } from '../protocol';
import type { AgentRow, HomeWire, ProjectWire } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import {
  accountChip,
  compactionFigure,
  copyReason,
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
  railFooter,
  railGroups,
  railMark,
  type RailGroup,
  type RailProject,
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
  /**
   * The occupant's id rides the header, and it is the reason the field exists:
   * a page attached to a running seat hears no `Connected`, so nothing else on
   * the wire names the session it is showing.
   */
  it("carries the occupant's id, and nothing where the seat has none", () => {
    const named = sessionFrom({
      ...(session as unknown as Record<string, unknown>),
      header: { ...record.header, session_id: 'd4f70669-1f2a' },
    });
    expect(named.header.session_id).toBe('d4f70669-1f2a');
    expect(headerFacts(named.header).sessionId).toBe('d4f70669-1f2a');

    const unstarted = sessionFrom({
      ...(session as unknown as Record<string, unknown>),
      header: { ...record.header, session_id: null },
    });
    expect(unstarted.header.session_id).toBeNull();

    // A value that is not a string is one this client cannot name, and a seat
    // it cannot name draws no id rather than the word `undefined`.
    const wrong = sessionFrom({
      ...(session as unknown as Record<string, unknown>),
      header: { ...record.header, session_id: 7 },
    });
    expect(wrong.header.session_id).toBeNull();
  });

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

/**
 * The compaction figure the header draws beside the context bar, from the
 * conversation's own count rather than the header's - a boundary frame is a
 * `compact_boundary` row in the transcript and the count is what it adds up to.
 */
describe('the compaction figure', () => {
  /**
   * **Nothing at zero, which is the terminal's rule for the same reason**: the
   * row already carries five facts, and a `0 compactions` on every fresh
   * session is noise that says nothing a session without a boundary has not
   * already said by being new.
   */
  it('says nothing for a session that has never compacted', () => {
    expect(compactionFigure(0), 'a session with no boundary claimed a count').toBeNull();
  });

  it('agrees its noun with the count', () => {
    expect(compactionFigure(1), 'one compaction read as plural').toBe('1 compaction');
    expect(compactionFigure(54)).toBe('54 compactions');
  });
});

/**
 * The copy control's two failure states, which must not read alike: a page
 * with no clipboard is the page's origin to fix, and a refused write is a
 * permission, so a name that said the same thing for both would send a reader
 * after the wrong one.
 */
describe('the copy control', () => {
  it('names the two failures apart, and says what the click does', () => {
    expect(copyReason('failed'), 'the two failures read alike').not.toBe(
      copyReason('no-clipboard'),
    );
    expect(copyReason('ready')).toContain('session id');
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

  /**
   * **A seat this client has just closed reads asleep AT ONCE** (#1712): the
   * reader's click is what moves it out of their working section, not the
   * seconds the core takes to shut it down. The mark is the client's own, so
   * the caller brings the predicate in.
   */
  it('counts a closing seat as asleep the moment it is closed', () => {
    const leadRow: AgentRow = { ...lead(), lifecycle: 'Running', pending: null, reason: null };
    const worker: AgentRow = { ...leadRow, slot: { ...leadRow.slot, label: 'w1' }, label: 'w1' };
    const home = withHome({ agents: [leadRow, worker] });

    const block = (groups: RailGroup[]): RailProject | undefined =>
      groups.flatMap((group) => group.projects).find((entry) => entry.name === 'proj');
    const working = block(railGroups(home, LEAD, 0, (slot) => slot.label === 'w1'));
    expect(
      working?.workers.map((row) => row.slot.label),
      'the closed worker stayed in the working rows',
    ).toEqual([]);
    expect(
      working?.sleeping.map((row) => row.slot.label),
      'the closed worker did not fold away',
    ).toEqual(['w1']);

    // A lead's close cascades, so both of its rows carry the mark; the
    // project's rank is its strongest row either way.
    const asleep = railGroups(home, LEAD, 0, () => true).find((group) => group.heading === 'asleep')
      ?.projects[0];
    expect(asleep?.name, "a closed lead's project stayed out of asleep").toBe('proj');
  });

  /**
   * A group that holds the seat the page is showing has to say so: arriving on
   * an asleep seat - a deep link, a click from the roster - draws the marked
   * row inside a closed fold otherwise, and the reader sees the count and no
   * sign of where they are.
   */
  it('says whether the seat the page is showing is one of the rows behind it', () => {
    const sleeping: AgentRow = { ...lead(), lifecycle: 'Sleeping', pending: null, reason: null };
    const home = withHome({ agents: [sleeping] });
    const asleep = (home: HomeWire, slot: SessionSlot): RailGroup | undefined =>
      railGroups(home, slot, 0).find((group) => group.heading === 'asleep');

    expect(asleep(home, LEAD)?.holds, 'the fold closed over the seat being shown').toBe(true);
    expect(
      asleep(home, { ...LEAD, project: 'elsewhere' })?.holds,
      'a group claimed to hold a seat it does not carry',
    ).toBe(false);
    expect(
      railGroups(homeWire, LEAD, 0).find((group) => group.heading === 'needs you')?.holds,
      'a group the page is showing claimed nothing',
    ).toBe(true);
  });

  /**
   * The asleep section folds, and its heading carries what it hides: a folded
   * section with no count reads as an empty one.
   */
  it('counts the rows the asleep heading hides', () => {
    const sleeping: AgentRow = { ...lead(), lifecycle: 'Sleeping', pending: null, reason: null };
    const worker: AgentRow = { ...sleeping, slot: { ...sleeping.slot, label: 'w1' }, label: 'w1' };
    const home = withHome({
      agents: [{ ...sleeping, slot: { ...sleeping.slot, label: 'lead' } }, worker],
    });
    const asleep = railGroups(home, LEAD, 0).find((group) => group.heading === 'asleep');

    expect(asleep?.hidden, 'the asleep heading hides nothing it does not count').toBe(2);
    expect(
      railGroups(homeWire, LEAD, 0).find((group) => group.heading === 'needs you')?.hidden,
      'a group that does not fold claimed a count',
    ).toBeNull();
  });

  /**
   * A project's sleeping workers fold behind one row of their own: a reader
   * working in a live project is not working in them.
   */
  it("folds a project's sleeping workers behind one row, keeping the awake ones", () => {
    const leadRow: AgentRow = { ...lead(), lifecycle: 'Running', pending: null, reason: null };
    const worker = (label: string, lifecycle: AgentRow['lifecycle']): AgentRow => ({
      ...leadRow,
      slot: { ...leadRow.slot, label },
      label,
      lifecycle,
    });
    const home = withHome({
      agents: [
        leadRow,
        worker('w1', 'Running'),
        worker('w2', 'Sleeping'),
        worker('w3', 'LoggedOut'),
      ],
    });
    const project = railGroups(home, LEAD, 0).find((group) => group.heading === 'working')
      ?.projects[0];

    expect(
      project?.workers.map((row) => row.slot.label),
      'an awake worker was folded away',
    ).toEqual(['w1']);
    expect(
      project?.sleeping.map((row) => row.slot.label),
      'the sleeping workers were not folded, in the order the roster lists them',
    ).toEqual(['w2', 'w3']);
    expect(project?.shown, 'folding its workers moved the project out of working').toBe('lead');
  });

  /**
   * The mark is on the ROW the page is showing, not on the project around it:
   * a project box lit its workers with it, so four rows looked selected and
   * none of them said which one was open.
   */
  it('marks the row of the seat being shown, not the project around it', () => {
    const leadRow: AgentRow = { ...lead(), lifecycle: 'Running', pending: null, reason: null };
    const worker: AgentRow = {
      ...leadRow,
      slot: { ...leadRow.slot, label: 'w1' },
      label: 'w1',
    };
    const home = withHome({ agents: [leadRow, worker] });
    const project = (slot: SessionSlot): RailProject | undefined =>
      railGroups(home, slot, 0)
        .flatMap((group) => group.projects)
        .find((entry) => entry.name === 'proj');

    expect(project(LEAD)?.shown, 'the lead seat was not the row marked').toBe('lead');
    expect(project({ ...LEAD, label: 'w1' })?.shown, 'the shown worker was not marked').toBe('w1');
    expect(
      project({ org: 'TestOrg', project: 'elsewhere', label: 'lead' })?.shown,
      'a project the page is not showing marked a row',
    ).toBeNull();
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
    // The sets ride the PROJECT's row and the liveness rides the home, which
    // is the split the section reads across: a subscription list on the home
    // is one nothing produces.
    const home = withHome({
      projects: [
        {
          ...project(),
          connectors: {
            gotify: [
              { applications: ['homelab', 'alerts'], min_priority: 4 },
              { applications: ['homelab'], min_priority: null },
            ],
            slack: [],
          },
        },
      ],
      connectors: {
        gotify: { connected: true },
        slack: { connected_workspaces: [], load_failed: false },
      },
    });
    const row = home.projects[0] ?? null;
    expect(gotifySection(home, row)?.rows[1]?.v, 'a set with no floor claimed one').toBe('any');
    expect(gotifySection(home, row)?.rows[0]?.v).toBe('homelab, alerts');
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
    const home = withHome({
      projects: [
        {
          ...project(),
          connectors: {
            gotify: [],
            slack: [
              { id: 's1', workspace: 'Acme', target: 'Mentions' },
              {
                id: 's2',
                workspace: 'Trust Machines',
                target: { Conversation: { id: 'C1', name: '#alerts', mode: 'All' } },
              },
            ],
          },
        },
      ],
      connectors: {
        gotify: { connected: false },
        slack: {
          connected_workspaces: [['Trust Machines', true]],
          load_failed: false,
        },
      },
    });
    const slack = slackSection(home, home.projects[0] ?? null);
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

/** The fixture's home with a chipped account and a usage snapshot behind it. */
const withPool = (snapshot: unknown): HomeWire =>
  withHome({
    projects: [{ ...project(), chip: { account_name: 'Acct', state: 'ready' } }],
    accounts: {
      ...homeWire.accounts,
      loading: [
        {
          display_name: 'Acct',
          state: 'ready',
          last_error: null,
          retry_after: null,
          auth: 'token',
        },
      ],
      usage: [{ display_name: 'Acct', snapshot }],
    },
  });

describe('the rail footer', () => {
  it('states the five figures a spend-billed account reports', () => {
    const footer = railFooter(
      withPool({
        source: 'OpenRouterKey',
        spend: { daily: 1.5, weekly: 10, monthly: 42, limit: 50 },
        balance: 12.25,
      }),
      LEAD,
    );
    expect(footer.figures).toEqual([
      { label: 'day', value: '$1.50', dim: false },
      { label: 'week', value: '$10.00', dim: false },
      { label: 'month', value: '$42.00', dim: false },
      { label: 'balance', value: '$12.25', dim: false },
      { label: 'cap', value: '$50.00', dim: false },
    ]);
  });

  /**
   * The terminal's own three states for a figure nobody has reported: `$-` for
   * a key that has not been probed, `not set` for one with no cap to fill, and
   * a dash when there is no snapshot at all. A `$0.00` is a reading, and forge
   * has none.
   */
  it('keeps the five rows a snapshot has not filled, rather than dropping any', () => {
    const cold = railFooter(withPool({ source: 'OpenRouterKey' }), LEAD);
    expect(cold.figures.map((figure) => figure.label)).toEqual([
      'day',
      'week',
      'month',
      'balance',
      'cap',
    ]);
    expect(cold.figures.map((figure) => figure.value)).toEqual([
      '$-',
      '$-',
      '$-',
      '$-',
      '\u{2014}',
    ]);

    const probed = railFooter(
      withPool({ source: 'OpenRouterKey', spend: { daily: 1, weekly: 2, monthly: 3 } }),
      LEAD,
    );
    expect(probed.figures.at(-1), 'an uncapped key claimed a cap').toEqual({
      label: 'cap',
      value: 'not set',
      dim: true,
    });
  });

  it('draws the windows rather than the figures for a window-billed account', () => {
    const footer = railFooter(
      withPool({
        source: 'Oauth',
        five_hour: { utilization: 68, reset_description: '1h 48m' },
        seven_day: { utilization: 24, reset_description: '2d 6h' },
      }),
      LEAD,
    );
    expect(footer.figures, 'a window-billed account drew money figures').toEqual([]);
    expect(footer.windows.map((window) => window.label)).toEqual(['5h', '7d']);
  });

  it('names the versions, and the newer CLI only when npm has one', () => {
    const footer = railFooter(withPool(null), LEAD);
    expect(footer.versions.forge).toBe(homeWire.forge_version_short);
    expect(footer.versions.socket, 'the protocol this app speaks').toBe(PROTOCOL_VERSION);
    expect(footer.versions.claude).toBe('1.0.0');
    expect(footer.versions.update, 'a newer claude went unstated').toBe('1.1.0');

    const level = withHome({ cli_version: { installed: '1.1.0', latest: '1.1.0' } });
    expect(
      railFooter(level, LEAD).versions.update,
      'an equal version claimed an update',
    ).toBeNull();
  });

  /** The account reads by its name, with the pool's own word for its health on
   * the dot beside it and nothing else: the chip's billing word is gone. */
  it('names the account and carries no billing word', () => {
    const footer = railFooter(withPool(null), LEAD);
    expect(footer.account).toEqual({ name: 'Acct', tone: 'ok' });
  });

  it('draws no footer account for a project that chips none', () => {
    expect(railFooter(homeWire, LEAD).account).toBeNull();
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
    expect(view?.name).toBe('Acct');
    expect(view?.tone, 'a pool that has not settled does not read as ready').toBe('wait');
    expect(view?.windows, 'a pool with no snapshot behind it drew windows').toEqual([]);
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

/**
 * The dictation axes a session has overridden, which the composer's panel
 * reads to know what is in force.
 *
 * **They are nested under `state`, which is where the server assembles them and
 * where its own committed fixture carries them** - this file's `session` is
 * that fixture's byte-pinned copy (`salvage.test.ts`), so the path here is the
 * wire's rather than one this test invented.
 */
describe('the dictation overrides', () => {
  /**
   * The fixture's record, with the axes a session set where the wire holds them.
   * The parameter is the wire's shape, not the narrowed one: a case below feeds
   * a value this client is older than.
   */
  const withOverrides = (overrides: Record<string, unknown>): Record<string, unknown> => ({
    ...session,
    state: { ...session.state, dictate_overrides: overrides },
  });

  it("does not read the session's overrides: the axes are this client's own", () => {
    // The wire still carries them for the terminal's `/dictate` overlay, and
    // this client captures its own audio - so the axes it dictates with are
    // held here, per seat, and the session's set reaches nothing.
    const held = sessionFrom(
      withOverrides({ styling: 'casual', structure: null, context: 'email' }),
    );
    expect(
      Object.hasOwn(held, 'dictate_overrides'),
      'the record must not carry a field nothing reads',
    ).toBe(false);
  });
});
