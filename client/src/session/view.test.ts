import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

import { FORGE_COMMANDS } from '../composer/forge-commands';
import { homeWire } from '../dev/fixture.data';
import session from '../dev/fixtures/session.json';
import { PROTOCOL_VERSION } from '../protocol';
import type { AgentRow, CronEntry, HomeWire, ProjectWire, Task } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import {
  accountChip,
  chipState,
  compactionFigure,
  paletteRows,
  copyReason,
  fleetCount,
  gitStrip,
  markOf,
  headerFacts,
  failedLine,
  mcpRows,
  mcpState,
  memoryLabel,
  monitorLabel,
  monitorRows,
  railFooter,
  railGroups,
  railMark,
  rankOf,
  type RailGroup,
  type RailProject,
  seatConnectorRows,
  seatScheduleRows,
  seatState,
  taskRows,
  untilOf,
} from './view';
import { sessionFrom, type SessionRecord } from './wire';

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

  /**
   * **The tree degrades where it enters too.** The fixture only ever holds
   * a complete tree, so nothing else exercises the arms a thinner or newer
   * server lands on: an unknown status wears the least-alarming class, a
   * commit list without a count states its own length, and a file the wire
   * does not name is dropped rather than drawn blank.
   */
  it('narrows an unknown status and fills what the tree does not state', () => {
    const tree = sessionFrom({
      ...(session as unknown as Record<string, unknown>),
      git: {
        default_branch: 'main',
        worktree: {
          files: [
            { path: 'a.rs', added: 1, removed: 0, status: 'quantum' },
            { added: 2, removed: 0, status: 'added' },
          ],
          total_files: 2,
          total_added: 3,
          total_removed: 0,
        },
        ahead: { commits: [{ sha: 'a1b2c3d', subject: 'one' }] },
      },
    });
    const files = tree.git.worktree?.files ?? [];
    expect(files[0]?.status, 'a status from the future did not narrow').toBe('modified');
    expect(files, 'a file the wire did not name was not dropped').toHaveLength(1);
    expect(tree.git.ahead?.count, 'a commit list without a count did not state its length').toBe(1);
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

    // A closing seat writes on no line: a worker closed while its ask was up
    // does not keep the project's line alive under the asleep block.
    const held: AgentRow = { ...worker, pending: 'question' };
    const closed = block(
      railGroups(withHome({ agents: [leadRow, held] }), LEAD, 0, (slot) => slot.label === 'w1'),
    );
    expect(closed?.why, 'a closed seat still wrote on the project line').toBeNull();
  });

  /**
   * A failed turn is the seat's own failure, and both surfaces say so from
   * one mapping: the row carries the line and the header takes the failure
   * mark. Each half had its own way to fall silent - the line through
   * `failedLine`, the mark through the promotion - so both are pinned.
   */
  it('names a failed turn on the row and marks it for the header', () => {
    const at = { secs_since_epoch: 1_800_000_000, nanos_since_epoch: 0 };
    const failed: AgentRow = { ...lead(), pending: null, failed_turn: at };
    const home = withHome({ agents: [failed] });

    const block = railGroups(home, LEAD, 0)[0]?.projects[0];
    if (block === undefined) throw new Error('the rail drew no block for the failed seat');
    expect(failedLine(block.row), 'the row names the failure').toBe('a turn failed');
    expect(block.why, 'and the line draws under the row').toEqual({
      line: 'a turn failed',
      bad: true,
    });
    expect(seatState(home, LEAD).mark, 'the header draws the failure mark').toBe('failed');
  });

  /**
   * **A failure line is the failing seat's own.** A worker's spawn diagnostic
   * searched onto the project's line put it under the lead's row, reading as
   * the lead having failed while the worker's own row said nothing.
   */
  it("keeps a worker's failure off the project's line", () => {
    const leadRow: AgentRow = { ...lead(), lifecycle: 'Running', pending: null, reason: null };
    const failed: AgentRow = {
      ...leadRow,
      slot: { ...leadRow.slot, label: 'client-dev' },
      label: 'client-dev',
      lifecycle: 'Failed',
      reason: 'transport closed before initialize',
    };
    const needs = railGroups(withHome({ agents: [leadRow, failed] }), LEAD, 0).find(
      (group) => group.heading === 'needs you',
    );

    expect(needs?.projects[0]?.why, "a worker's failure rode the project's line").toBeNull();
  });

  /**
   * The lead's own failure IS the project's: a dead lead is what the project's
   * line is for, and a failure the core left no text for still says something.
   */
  it("draws the lead's own failure on the project's line", () => {
    const failed: AgentRow = {
      ...lead(),
      lifecycle: 'Failed',
      pending: null,
      reason: 'the subprocess exited',
    };
    const why = (home: HomeWire): RailProject['why'] | undefined =>
      railGroups(home, LEAD, 0).find((group) => group.heading === 'needs you')?.projects[0]?.why;

    expect(why(withHome({ agents: [failed] })), "the lead's failure lost its text").toEqual({
      line: 'the subprocess exited',
      bad: true,
    });
    expect(
      why(withHome({ agents: [{ ...failed, reason: null }] })),
      'a failure with no recorded text claimed nothing',
    ).toEqual({ line: 'spawn failed', bad: true });
    expect(
      why(withHome({ agents: [{ ...failed, lifecycle: 'AuthRequired', reason: null }] })),
      'a seat waiting on sign-in claimed a spawn failed',
    ).toEqual({ line: 'not running', bad: true });
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
    // A failed turn is the same failure shape, and it ranks with the
    // states that need the reader rather than with the completions.
    expect(railMark({ kind: 'failed-turn' })).toBe('failed');
    expect(rankOf({ kind: 'failed-turn' }, null)).toBe(0);
    expect(rankOf({ kind: 'failed-turn' }, 'question')).toBe(0);
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

describe('the tree the strip draws', () => {
  /** The fixture's record with the scan's own view laid over it. */
  const withGit = (
    git: Partial<SessionRecord['git']>,
    over: Partial<SessionRecord> = {},
  ): SessionRecord => ({
    ...record,
    git: { defaultBranch: 'main', worktree: null, ahead: null, ...git },
    ...over,
  });

  it('leads with the branch, and carries the tree whole: chains, files, marks', () => {
    const stats = {
      files: [
        { path: 'client/src/lib.rs', added: 12, removed: 4, status: 'modified' as const },
        { path: 'docs/new.md', added: 3, removed: 0, status: 'added' as const },
      ],
      totalFiles: 2,
      totalAdded: 15,
      totalRemoved: 4,
    };
    const strip = gitStrip(
      withGit(
        {
          worktree: stats,
          ahead: {
            count: 2,
            commits: [
              { sha: 'a1b2c3d', subject: 'the first commit', stats: null, time: 1_766_000_000 },
              { sha: 'd4e5f6a', subject: 'the second commit', stats: null, time: 1_766_000_100 },
            ],
            stats,
          },
        },
        {
          work: { branch: 'web-home-layout', changed: 2, gate: 'in_repo' },
        },
      ),
      LEAD,
    );

    expect(strip?.label, 'where the branch runs, how far, on which PR, and that it is dirty').toBe(
      'web-home-layout \u{b7} 2 commits \u{b7} PR #1249 \u{b7} dirty',
    );
    expect(strip?.head, 'what the tree IS leads the hover').toBe("the project's tree");
    expect(strip?.ahead, 'the chain, its count, its range and the branch it is ahead of').toEqual({
      count: 2,
      base: 'main',
      commits: [
        { sha: 'a1b2c3d', subject: 'the first commit', stats: null, time: 1_766_000_000 },
        { sha: 'd4e5f6a', subject: 'the second commit', stats: null, time: 1_766_000_100 },
      ],
      stats,
    });
    expect(strip?.uncommitted, 'the uncommitted files with their marks and totals').toEqual(stats);
  });

  it('states the pull request with its state and what it closes', () => {
    const strip = gitStrip(
      withGit(
        {},
        {
          work: { branch: 'web-home-layout', changed: 0, gate: 'in_repo' },
          pr: { number: 1203, url: 'https://example.test/pull/1203', draft: true },
          closes: [{ number: 1200, url: 'https://example.test/issues/1200' }],
        },
      ),
      LEAD,
    );

    expect(strip?.pr).toEqual({
      number: 1203,
      url: 'https://example.test/pull/1203',
      draft: true,
      closes: '#1200',
    });
    expect(strip?.label, 'the toggle names the PR it is on').toContain('PR #1203');
  });

  /**
   * A seat outside a repository has no branch and no count, and the gate line
   * is then the whole of what the row says under its toggle. Drawing nothing
   * would read as a seat with nothing to report rather than as a tree that
   * could not be read.
   */
  it('draws the reason a tree could not be read', () => {
    const strip = gitStrip(
      withGit({}, { work: { branch: null, changed: null, gate: 'gone' }, pr: null, closes: [] }),
      LEAD,
    );
    expect(strip?.label, 'the toggle says there is no branch, not why').toBe('no branch');
    expect(strip?.gate).toBe('its working directory is not there');
  });

  /**
   * **On the default branch with nothing on it there is no row at all.**
   * The branch everything lands on with a clean tree is where work goes, not
   * work: a row there would state that nothing is happening, on every seat,
   * forever. A dirty default branch still draws - there is something to see.
   */
  it('hides itself on the default branch, and draws once the tree is dirty', () => {
    const clean = gitStrip(
      withGit({}, { work: { branch: 'main', changed: 0, gate: 'in_repo' }, pr: null, closes: [] }),
      LEAD,
    );
    expect(clean, 'a clean default branch drew a row').toBeNull();

    // In a clone the default arrives as `origin/main` while the checked-out
    // branch is plain `main`: the same row must stay hidden.
    const clone = gitStrip(
      withGit(
        { defaultBranch: 'origin/main' },
        { work: { branch: 'main', changed: 0, gate: 'in_repo' }, pr: null, closes: [] },
      ),
      LEAD,
    );
    expect(clone, 'a clone on main with a clean tree drew a row').toBeNull();

    // A clean default branch holding an open pull request still draws: the
    // PR is state the row exists for.
    const withPr = gitStrip(
      withGit(
        {},
        {
          work: { branch: 'main', changed: 0, gate: 'in_repo' },
          pr: { number: 1203, url: 'https://example.test/pull/1203', draft: false },
          closes: [],
        },
      ),
      LEAD,
    );
    expect(withPr, 'a default branch holding a PR drew no row').not.toBeNull();

    const dirty = gitStrip(
      withGit(
        {
          worktree: {
            files: [{ path: 'a.rs', added: 1, removed: 0, status: 'modified' }],
            totalFiles: 1,
            totalAdded: 1,
            totalRemoved: 0,
          },
        },
        { work: { branch: 'main', changed: 1, gate: 'in_repo' }, pr: null, closes: [] },
      ),
      LEAD,
    );
    expect(dirty, 'a dirty default branch went unstated').not.toBeNull();
    expect(dirty?.uncommitted?.files).toHaveLength(1);
  });

  it('names the worktree a seat is on, and whose tree it is when it is not one', () => {
    const worktree = gitStrip(
      {
        ...withGit({}, { work: { branch: 'work/schedule-row', changed: 0, gate: 'in_repo' } }),
        state: { scan_cwd: '/w/forge/.claude/worktrees/session-design' },
        pr: null,
        closes: [],
      },
      { ...LEAD, label: 'session-design' },
    );
    expect(worktree?.head).toBe('worktree \u{b7} session-design');

    const worker = gitStrip(
      withGit(
        {},
        { work: { branch: 'feat/x', changed: 0, gate: 'in_repo' }, pr: null, closes: [] },
      ),
      { ...LEAD, label: 'builder' },
    );
    expect(worker?.head, "a worker's own path is still the worker's").toBe("a worker's tree");
  });

  it('reads a commit count of one in the singular', () => {
    const strip = gitStrip(
      withGit(
        { ahead: { count: 1, commits: [], stats: null } },
        { work: { branch: 'feat/x', changed: 1, gate: 'in_repo' }, pr: null, closes: [] },
      ),
      LEAD,
    );
    expect(strip?.label, 'one commit reads as one').toBe('feat/x \u{b7} 1 commit \u{b7} dirty');
  });

  it('maps every status to the mark the terminal draws for it', () => {
    expect(markOf('modified')).toEqual({ letter: 'M', klass: 'mark' });
    expect(markOf('added')).toEqual({ letter: 'A', klass: 'ok' });
    expect(markOf('deleted')).toEqual({ letter: 'D', klass: 'bad' });
    expect(markOf('renamed')).toEqual({ letter: 'R', klass: 'mark' });
    expect(markOf('copied')).toEqual({ letter: 'C', klass: 'mark' });
    expect(markOf('typechange')).toEqual({ letter: 'T', klass: 'mark' });
    expect(markOf('unmerged')).toEqual({ letter: '!', klass: 'bad' });
    expect(markOf('untracked')).toEqual({ letter: 'U', klass: 'warn' });
  });
});

describe('the schedule countdown', () => {
  it('reads a time already past as due rather than counting into the past', () => {
    expect(
      untilOf({ secs_since_epoch: 0 }, 1_700_000_000_000),
      'a passed fire read as future',
    ).toBe('due now');
    expect(untilOf({ secs_since_epoch: 1_700_003_600 }, 1_700_000_000_000)).toBe('in 1h');
    expect(untilOf(null, 0)).toBe('due now');
  });
});

describe('the tasks the strip draws', () => {
  it("scopes the rows to the seat, the way the terminal's own section does", () => {
    const base = {
      project_name: 'proj',
      active_form: null,
      detail: null,
      parent: null,
      artifact: null,
      estimate: null,
      created_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
      updated_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
    };
    const tasks: Task[] = [
      { ...base, id: 'top', subject: 'the campaign', status: 'in_progress', owner: null },
      {
        ...base,
        id: 'mine',
        subject: 'my row',
        status: 'in_progress',
        owner: { org: 'TestOrg', project: 'proj', label: 'builder' },
        parent: 'top',
      },
      {
        ...base,
        id: 'child',
        subject: 'a child row',
        status: 'pending',
        owner: null,
        parent: 'top',
      },
    ];

    // A lead draws its campaign board: top-level rows only, so the child of
    // one is not among them.
    const board = taskRows(tasks, LEAD);
    expect(
      board.map((row) => row.id),
      'the lead drew a row that is not top-level',
    ).toEqual(['top']);
    // A worker draws only what it owns.
    const own = taskRows(tasks, { org: 'TestOrg', project: 'proj', label: 'builder' });
    expect(
      own.map((row) => row.id),
      'a worker drew a row that is not its own',
    ).toEqual(['mine']);
  });

  it('reads in-progress first, and keys each row by its own id', () => {
    const rows = taskRows(
      [
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
          active_form: 'Going still',
          detail: null,
          status: 'in_progress',
          owner: LEAD,
          parent: null,
          artifact: 'https://example.test/pull/1204',
          estimate: '2h',
          created_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
          updated_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
        },
      ],
      LEAD,
    );
    expect(rows[0]?.subject).toBe('still going');
    expect(rows[0]?.display, 'a running row leads with its active form').toBe('Going still');
    expect(rows[1]?.display, 'and a settled one keeps its subject').toBe('done already');
    expect(rows[0]?.status, 'in progress leads, and the row draws its own mark').toBe(
      'in_progress',
    );
    expect(rows[0]?.id, 'the row is keyed by the task its own id').toBe('t2');
    expect(rows[0]?.owner, 'the owner rides its own cell').toBe('lead');
    expect(rows[0]?.meta).toBe('in progress \u{b7} PR 1204 \u{b7} 2h');
    expect(rows[1]?.status).toBe('completed');
  });
});

describe('the projects chip', () => {
  const agent = (label: string, over: Partial<AgentRow> = {}): AgentRow => ({
    slot: { org: 'TestOrg', project: 'proj', label },
    label,
    lifecycle: 'Idle',
    has_background_work: false,
    pending: null,
    pending_depth: 0,
    last_activity: null,
    reason: null,
    failed_turn: null,
    work: null,
    ...over,
  });

  /**
   * **Never a mere change.** A busy or idle seat counts for nothing; the
   * chip is only the seats that want a PERSON, and one such seat - with no
   * failure among them - is the case its click goes straight to it.
   */
  it('counts only the seats that want a person', () => {
    const wire = withHome({
      agents: [agent('busy'), agent('waiter', { pending: 'permission' })],
    });
    expect(chipState(wire), 'a seat that does not want a person counted').toEqual({
      state: 'one',
      count: 1,
      href: '/session/TestOrg/proj/waiter',
      label: '1 seat needs you',
    });
  });

  /** Several, any failure among them, or none: the home is where you pick. */
  it('goes home for several, for any failure and for none', () => {
    const many = chipState(
      withHome({
        agents: [agent('a', { pending: 'permission' }), agent('b', { pending: 'permission' })],
      }),
    );
    expect(many.state, 'two wanting seats did not read as several').toBe('many');
    expect(many.href, 'several did not go home').toBe('/');
    expect(many.label).toBe('2 seats need you');

    const failed = chipState(
      withHome({
        agents: [agent('a', { failed_turn: { secs_since_epoch: 1, nanos_since_epoch: 0 } })],
      }),
    );
    expect(failed, 'a lone failure did not go home').toMatchObject({
      state: 'failed',
      href: '/',
      count: 1,
    });
    expect(failed.label, 'the failure is not in the accessible name').toBe(
      '1 seat needs you, one failed',
    );

    const none = chipState(withHome({ agents: [agent('busy')] }));
    expect(none, 'a quiet fleet did not read as the calm word').toEqual({
      state: 'none',
      count: 0,
      href: '/',
      label: 'projects',
    });
  });
});

describe("the palette's rows", () => {
  const agent = (label: string, over: Partial<AgentRow> = {}): AgentRow => ({
    slot: { org: 'TestOrg', project: 'proj', label },
    label,
    lifecycle: 'Idle',
    has_background_work: false,
    pending: null,
    pending_depth: 0,
    last_activity: null,
    reason: null,
    failed_turn: null,
    work: null,
    ...over,
  });

  /**
   * A project's identity is (org, name) here as everywhere else: a namesake
   * in another org must not wear the chip, or Enter lands in the wrong org's
   * seat.
   */
  it('marks the lead by its org and project both', () => {
    const wire = withHome({
      agents: [
        agent('lead', { slot: { org: 'Other', project: 'proj', label: 'lead' } }),
        agent('lead'),
      ],
    });
    const leads = paletteRows(wire, LEAD)
      .flatMap((section) => section.rows)
      .filter((row) => row.lead === true);
    expect(leads, 'two orgs sharing a project name both wore the chip').toHaveLength(1);
    expect(leads[0]?.id, 'the wrong org wore it').toBe('TestOrg/proj/lead');
  });

  /**
   * **The fleet grouped its own way**: the seats that want a person, the ones
   * working, the ones asleep - then forge's commands, then the doings. The
   * current project's lead wears `lead`, which is where the cursor starts, so
   * Cmd+K then Enter lands on it.
   */
  it('groups needs-you, working, asleep, commands and doings, in order', () => {
    // The grouping IS rankOf's: an idle live session ranks as working (the
    // rail draws it so), and only a sleeping or logged-out one is asleep.
    const wire = withHome({
      agents: [
        agent('waiter', { lifecycle: 'Attention', pending: 'permission' }),
        agent('runner', { lifecycle: 'Running' }),
        agent('dozer', { lifecycle: 'Sleeping' }),
        agent('lead'),
      ],
    });
    const sections = paletteRows(wire, LEAD);
    expect(sections.map((section) => section.title)).toEqual([
      'needs you',
      'working',
      'asleep',
      'commands',
      'doings',
    ]);
    expect(sections[0]?.rows.map((row) => row.label)).toEqual(['waiter']);
    expect(sections[0]?.rows[0]?.mark, 'a needing seat wears the needs mark').toBe('needs');
    expect(sections[1]?.rows.map((row) => row.label)).toEqual(['runner', 'lead']);
    expect(sections[2]?.rows.map((row) => row.label)).toEqual(['dozer']);
    const leadRow = sections[1]?.rows.find((row) => row.label === 'lead');
    expect(leadRow?.lead, 'the lead is the row the cursor starts on').toBe(true);
    expect(leadRow?.href).toBe('/session/TestOrg/proj/lead');
    // The command rows are the composer's own table, whole: a command the
    // box offers cannot be missing here, and nothing extra rides along.
    const commands = sections[3]?.rows ?? [];
    expect(
      commands.map((row) => row.label),
      'the palette and the box disagree on the command set',
    ).toEqual(FORGE_COMMANDS.map((command) => command.name));
    const compact = commands.find((row) => row.label === '/compact');
    expect(compact?.text, 'a command row carries its prompt text').toBe('/compact');
    const doings = sections[4]?.rows.map((row) => row.label) ?? [];
    expect(doings).toContain('peek at the fleet');
    expect(doings).toContain('close this seat');
    const home = sections[4]?.rows.find((row) => row.label === 'go home');
    expect(home?.href, 'the home doing is a href').toBe('/');
  });

  /**
   * **The table is commands.rs's, and this reading keeps them equal.** The
   * client's copy is static so the box opens without a round trip; a static
   * copy drifts silently, so the pin reads the Rust side.
   */
  it('keeps the command table equal to commands.rs', () => {
    const rust = readFileSync(
      new URL('../../../crates/forge-server/src/commands.rs', import.meta.url),
      'utf8',
    );
    const names = [...rust.matchAll(/name:\s*"(\/[a-z]+)"/g)].map((match) => match[1] ?? '');
    expect(names.length, 'the Rust table parsed to nothing').toBeGreaterThan(0);
    expect(
      FORGE_COMMANDS.map((command) => command.name),
      'the client table drifted from commands.rs',
    ).toEqual(names);
  });

  /** A logged-out seat reads asleep, the same way a sleeping one does. */
  it('reads a logged-out seat as asleep', () => {
    const wire = withHome({ agents: [agent('gone', { lifecycle: 'LoggedOut' })] });
    const row = paletteRows(wire, LEAD)[0]?.rows[0];
    expect(row?.detail, 'a logged-out seat did not read asleep').toContain('asleep');
  });
});

describe('process figures', () => {
  it('reads memory in the unit the reader thinks in', () => {
    expect(memoryLabel(0)).toBe('0 B');
    expect(memoryLabel(1024)).toBe('1 KB');
    expect(memoryLabel(1024 * 1024)).toBe('1 MB');
    expect(memoryLabel(1024 * 1024 * 1024)).toBe('1.0 GB');
  });
});

describe('the monitors the strip draws', () => {
  it('keys each row by its own call and states whether it is running', () => {
    const rows = monitorRows(
      [
        {
          tool_use_id: 'm1',
          task_id: null,
          description: 'ci-watch',
          command: 'gh run watch',
          persistent: true,
          timeout_ms: 0,
          status: 'running',
          output_file: null,
          ended_at: null,
        },
        {
          tool_use_id: 'm2',
          task_id: null,
          description: 'log-tail',
          command: 'tail -f forge.log',
          persistent: false,
          timeout_ms: 0,
          status: 'timed_out',
          output_file: null,
          ended_at: null,
        },
        {
          tool_use_id: 'm3',
          task_id: null,
          description: 'done',
          command: 'true',
          persistent: false,
          timeout_ms: 0,
          status: 'completed',
          output_file: null,
          ended_at: null,
        },
      ],
      0,
    );
    expect(rows[0]).toEqual({
      id: 'm1',
      running: true,
      completed: false,
      name: 'ci-watch',
      label: 'persistent',
      command: 'gh run watch',
    });
    expect(rows[1]?.running, 'a settled monitor does not read as live').toBe(false);
    expect(rows[1]?.completed, 'a timed-out watch did not complete').toBe(false);
    expect(rows[1]?.label).toBe('timed out');
    expect(rows[2]?.completed, 'a completed watch did').toBe(true);
  });

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
  /**
   * The pair, both sides stated whether or not they agree: a mismatch is
   * then a difference the reader sees, not the absence of a notice. The
   * `skewed` flag is that difference, stated once.
   */
  it('states the server and client protocol pair, and marks a mismatch', () => {
    const agreed = railFooter(homeWire, LEAD, PROTOCOL_VERSION);
    expect(agreed.versions).toMatchObject({
      serverProtocol: PROTOCOL_VERSION,
      clientProtocol: PROTOCOL_VERSION,
      skewed: false,
    });

    const oneBack = railFooter(homeWire, LEAD, PROTOCOL_VERSION - 1);
    expect(oneBack.versions.serverProtocol, 'the server did not state its own').toBe(
      PROTOCOL_VERSION - 1,
    );
    expect(oneBack.versions.skewed, 'a mismatch read as agreement').toBe(true);

    const early = railFooter(homeWire, LEAD);
    expect(early.versions.serverProtocol, 'no greeting yet claimed a protocol').toBeNull();
    expect(early.versions.skewed).toBe(false);
  });

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
    expect(footer.versions.serverForge).toBe(homeWire.forge_version_short);
    expect(footer.versions.clientProtocol, 'the protocol this app speaks').toBe(PROTOCOL_VERSION);
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

describe('the MCP rows the strip draws', () => {
  it('states the state, and leaves the reason to the row line beneath', () => {
    expect(mcpState({ name: 'forge', status: 'failed', error: '  ' })).toBe('failed');
    expect(
      mcpState({ name: 'forge', status: 'failed', error: ' the CLI refused ' }),
      'the reason drew in the state cell as well as its own line',
    ).toBe('failed');
    expect(mcpState({ name: 'forge', status: 'connected', tools: [] })).toBe('no tools');
    expect(mcpState({ name: 'forge', status: 'connected', tools: [{}, {}] })).toBe('2 tools');
  });

  it('carries the status detail a server row draws: tools, command, reason', () => {
    const held = sessionFrom({
      ...(session as unknown as Record<string, unknown>),
      mcp: {
        error: null,
        servers: [
          {
            name: 'context7',
            status: 'connected',
            config: { type: 'stdio', command: 'npx', args: ['-y', '@upstash/context7-mcp'] },
            tools: [
              { name: 'query-docs', description: 'Ask the docs' },
              { name: 'resolve-library-id' },
            ],
          },
          {
            name: 'forge',
            status: 'failed',
            error: ' the CLI refused ',
            config: { type: 'http', url: 'https://mcp.example.test' },
          },
        ],
      },
    });

    const rows = mcpRows(held);

    expect(rows, 'one row per server').toHaveLength(2);
    expect(rows[0], 'a connected server names its tools and its backing command').toMatchObject({
      name: 'context7',
      k: 'context7 \u{b7} session',
      v: '2 tools',
      tools: ['query-docs', 'resolve-library-id'],
      command: 'npx -y @upstash/context7-mcp',
      reason: null,
      synthetic: false,
    });
    expect(rows[1], 'a failed server carries its reason and reaches its URL').toMatchObject({
      name: 'forge',
      k: 'forge \u{b7} session',
      v: 'failed',
      command: 'https://mcp.example.test',
      reason: 'the CLI refused',
    });
  });

  it('draws nothing for a session that reported nothing, and the failure for a read that failed', () => {
    const bare = sessionFrom({
      ...(session as unknown as Record<string, unknown>),
      mcp: { error: null, servers: [] },
    });
    expect(mcpRows(bare), 'no servers and no failure: no rows at all').toEqual([]);
    expect(mcpRows(null), 'a record that has not landed draws nothing').toEqual([]);

    const refused = sessionFrom({
      ...(session as unknown as Record<string, unknown>),
      mcp: { error: 'the CLI refused', servers: [] },
    });
    const rows = mcpRows(refused);
    expect(rows, 'an empty read that failed is a state of its own').toHaveLength(1);
    expect(rows[0], 'and it draws as itself, with the reason').toMatchObject({
      k: 'servers',
      v: 'failed',
      reason: 'the CLI refused',
      synthetic: true,
    });
  });
});

describe("a seat's own connector rows", () => {
  const WORKER: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'builder' };

  /** A home whose connectors are up, so a row's value carries no state
   *  suffix unless a case takes the stream down. */
  const live = (change: Record<string, unknown> = {}): Partial<HomeWire> => ({
    connectors: {
      gotify: { connected: true },
      slack: { connected_workspaces: [['forge', true]] },
      ...change,
    },
  });

  const home = (change: Record<string, unknown> = {}) =>
    withHome({
      ...live(),
      ...change,
      projects: [
        {
          ...project(),
          connectors: {
            gotify: [
              { id: 'g-1', applications: ['client-alerts'], min_priority: 5, team_role: null },
              { id: 'g-2', applications: [], min_priority: null, team_role: 'builder' },
            ],
            slack: [
              { id: 's-1', workspace: 'forge', target: 'Mentions', team_role: null },
              {
                id: 's-2',
                workspace: 'forge',
                target: { Conversation: { name: 'field-notes', mode: 'All' } },
                team_role: 'builder',
              },
            ],
          },
        },
      ],
    });

  it('reads the lead the no-owner subscriptions and nothing else', () => {
    const rows = seatConnectorRows(home(), LEAD);

    expect(rows, 'the lead owns one of each').toHaveLength(2);
    expect(rows[0], 'its gotify apps and floor, keyed by the subscription own id').toEqual({
      kind: 'gotify',
      id: 'g-1',
      key: 'client-alerts',
      value: '>=5',
    });
    expect(rows[1], 'and its slack target, the bare-string arm read by name').toEqual({
      kind: 'slack',
      id: 's-1',
      key: 'forge',
      value: 'mentions anywhere \u{b7} mentions only',
    });
  });

  it("reads a worker its own label and never the lead's", () => {
    const rows = seatConnectorRows(home(), WORKER);

    expect(rows, 'the worker owns one of each').toHaveLength(2);
    expect(rows[0], 'its own gotify row: no app filter, any priority').toEqual({
      kind: 'gotify',
      id: 'g-2',
      key: 'any app',
      value: 'any priority',
    });
    expect(rows[1], 'and its own slack conversation, named and readable').toEqual({
      kind: 'slack',
      id: 's-2',
      key: 'forge',
      value: 'field-notes \u{b7} every message',
    });
  });

  it('keys by the subscription, so two subs sharing one workspace both draw', () => {
    // The live crash shape: a mentions watcher + the auto-subscribed
    // conversation in one workspace read the same drawn words, and a row
    // keyed by those words throws in dev and prod alike.
    const two = withHome({
      ...live(),
      projects: [
        {
          ...project(),
          connectors: {
            gotify: [],
            slack: [
              { id: 's-1', workspace: 'forge', target: 'Mentions', team_role: null },
              {
                id: 's-2',
                workspace: 'forge',
                target: { Conversation: { name: 'forge', mode: 'All' } },
                team_role: null,
              },
            ],
          },
        },
      ],
    });

    const rows = seatConnectorRows(two, LEAD);

    expect(rows, 'both subscriptions draw').toHaveLength(2);
    expect(new Set(rows.map((row) => row.id)).size, 'and their ids are distinct').toBe(2);
  });

  it('reads only the seat project, never another project on the home', () => {
    // The OTHER project comes FIRST on purpose: with the seat's project
    // first, a lookup that took `projects[0]` would pass this whole file.
    const two = withHome({
      ...live(),
      projects: [
        {
          ...project(),
          project: { ...project().project, name: 'other', key: 'TestOrg-other' },
          connectors: {
            gotify: [],
            slack: [{ id: 's-x', workspace: 'Acme', target: 'Mentions', team_role: null }],
          },
        },
        {
          ...project(),
          connectors: {
            gotify: [],
            slack: [{ id: 's-1', workspace: 'forge', target: 'Mentions', team_role: null }],
          },
        },
      ],
    });

    const rows = seatConnectorRows(two, LEAD);

    expect(rows, 'one row, from the seat project').toHaveLength(1);
    expect(rows[0]?.key, "another project's channel drew on this seat").toBe('forge');
  });

  it('names a slack read that failed, which is the only surface left saying so', () => {
    // `load_failed` is the boot that could not read the durable slack
    // subscriptions: the project row draws nothing, so without a row here the
    // failure would be invisible in the web client entirely.
    const failed = withHome({
      connectors: {
        gotify: { connected: true },
        slack: { connected_workspaces: [], load_failed: true },
      },
      projects: [
        {
          ...project(),
          connectors: { gotify: [], slack: [] },
        },
      ],
    });

    const rows = seatConnectorRows(failed, LEAD);

    expect(rows, 'the failure draws as a row of its own').toHaveLength(1);
    expect(rows[0], 'naming what failed').toEqual({
      kind: 'slack',
      id: 'slack-load',
      key: 'slack',
      value: 'subscriptions failed to load',
    });
  });

  it('reads the direct-messages arm by name, mode and all', () => {
    const dm = withHome({
      ...live(),
      projects: [
        {
          ...project(),
          connectors: {
            gotify: [],
            slack: [{ id: 's-1', workspace: 'forge', target: 'DirectMessages', team_role: null }],
          },
        },
      ],
    });

    const rows = seatConnectorRows(dm, LEAD);

    expect(rows[0]?.value, 'a bare-string arm must not draw an empty key').toBe(
      'direct messages \u{b7} every message',
    );
  });

  it('says so on the row when the stream behind it is not up', () => {
    const down = withHome({
      connectors: {
        gotify: { connected: false },
        slack: { connected_workspaces: [] },
      },
      projects: [
        {
          ...project(),
          connectors: {
            gotify: [{ id: 'g-1', applications: [], min_priority: null, team_role: null }],
            slack: [{ id: 's-1', workspace: 'forge', target: 'Mentions', team_role: null }],
          },
        },
      ],
    });

    const rows = seatConnectorRows(down, LEAD);

    expect(rows[0]?.value, 'a disconnected gotify stream reads on its row').toBe(
      'any priority \u{b7} offline',
    );
    expect(rows[1]?.value, 'an unconnected workspace reads on its row').toBe(
      'mentions anywhere \u{b7} mentions only \u{b7} not connected',
    );
  });
});

describe("the project's schedule rows", () => {
  const NOW = 1_700_000_000_000;
  const cron = (over: Partial<CronEntry> = {}): CronEntry => ({
    id: 'c-1',
    project_name: 'proj',
    kind: { Recurring: '0 9 * * *' },
    prompt: 'sweep the rules',
    description: 'rules sweep',
    created_at: { secs_since_epoch: 1_699_000_000, nanos_since_epoch: 0 },
    // 27 days past NOW: the countdown in the assertions below.
    next_fire: { secs_since_epoch: 1_702_332_800, nanos_since_epoch: 0 },
    ...over,
  });

  it('names a schedule by its description and states its countdown and kind', () => {
    const home = withProject({ crons: [cron()] });

    const rows = seatScheduleRows(home, LEAD, NOW);

    expect(rows, 'one schedule').toHaveLength(1);
    expect(rows[0], 'the description leads, the countdown and kind follow').toEqual({
      id: 'c-1',
      key: 'rules sweep',
      value: 'in 27d \u{b7} recurring',
    });
  });

  it("falls back to the prompt's first line, and reads a one-shot as one", () => {
    // No description at all, which is the legacy shape the fallback is for.
    const bare = cron({
      id: 'c-2',
      prompt: 'audit the plugins\nand then report',
      // `Once` carries the instant, so it crosses as an object rather than a
      // bare variant name.
      kind: { Once: { secs_since_epoch: 1_702_332_800, nanos_since_epoch: 0 } },
    });
    delete bare.description;
    const home = withProject({ crons: [bare] });

    const rows = seatScheduleRows(home, LEAD, NOW);

    expect(rows[0]?.key, 'the first line of the prompt, not all of it').toBe('audit the plugins');
    expect(rows[0]?.value, 'a one-shot is a one-shot').toBe('in 27d \u{b7} one-shot');
  });

  /**
   * **A cron names the seat that created it** (`team_role`, absent for the
   * lead), so the page keeps its own label's set - the same ownership rule
   * the connector row applies.
   */
  it("keeps another seat's crons off this page, and this seat's own on it", () => {
    const home = withProject({
      crons: [cron(), cron({ id: 'c-w1', description: 'the worker sweep', team_role: 'w1' })],
    });

    const lead = seatScheduleRows(home, LEAD, NOW);
    expect(
      lead.map((row) => row.key),
      "a worker's cron drew on the lead's page",
    ).toEqual(['rules sweep']);

    const worker = seatScheduleRows(home, { ...LEAD, label: 'w1' }, NOW);
    expect(
      worker.map((row) => row.key),
      "the lead's cron drew on a worker's page",
    ).toEqual(['the worker sweep']);
  });

  it('keeps two schedules reading the same words apart by their ids', () => {
    const home = withProject({
      crons: [cron(), cron({ id: 'c-9' })],
    });

    const rows = seatScheduleRows(home, LEAD, NOW);

    expect(rows, 'both schedules draw').toHaveLength(2);
    expect(new Set(rows.map((row) => row.id)).size, 'with distinct ids to key by').toBe(2);
  });

  it('reads only the seat project, whatever the order on the home', () => {
    // The OTHER project comes first on purpose, so a lookup that took
    // `projects[0]` would read the wrong schedules.
    const home = withHome({
      projects: [
        {
          ...project(),
          project: { ...project().project, name: 'other', key: 'TestOrg-other' },
          crons: [cron({ id: 'c-x', description: 'the other project' })],
        },
        { ...project(), crons: [cron()] },
      ],
    });

    const rows = seatScheduleRows(home, LEAD, NOW);

    expect(rows, 'one row, from the seat project').toHaveLength(1);
    expect(rows[0]?.key, "another project's schedule drew on this seat").toBe('rules sweep');
  });
});
