import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

import type { AgentRow, HomeWire, Lifecycle } from '../wire/home';
import { homeWire } from '../wire/home';
import {
  artifactLabel,
  countsOf,
  elapsedLabel,
  followable,
  homeView,
  markOf,
  refusal,
  whenOf,
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

describe('a row over the fleet', () => {
  it('groups a project under its org and names the lead row for the project', () => {
    const view = homeView(homeWire, '127.0.0.1:8790');
    const org = view.orgs.find((section) => section.name === 'TestOrg');
    expect(org, 'the fixture project is grouped under its org').toBeDefined();
    expect(org?.projects).toHaveLength(1);
    // The lead's label is its identity, not what the row is called here.
    expect(org?.projects[0]?.lead.name).toBe('proj');
    expect(org?.projects[0]?.lead.slot.label).toBe('lead');
    expect(countsOf(org as NonNullable<typeof org>)).toBe('1 live');
  });

  it('reads the fixture the server pinned, without re-deriving any state', () => {
    const view = homeView(homeWire, 'ws://127.0.0.1:8790/socket');
    const lead = view.orgs[0]?.projects[0]?.lead;
    expect(lead?.state).toEqual({ kind: 'lifecycle', lifecycle: 'Idle' });
    expect(lead?.pending).toBe('permission');
    expect(view.header).toEqual({ liveAgents: 1, projects: 1, installed: '1.0.0', latest: '1.1.0' });
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
    const wire: HomeWire = {
      ...homeWire,
      agents: [],
      projects: [{ ...(homeWire.projects[0] as HomeWire['projects'][0]), sessions: [] }],
    };
    const view = homeView(wire, '');
    const lead = view.orgs[0]?.projects[0]?.lead;
    expect(lead?.state).toEqual({ kind: 'never-started' });
    expect(whenOf(lead as Row, Date.now())).toBe('never');

    // The same project with a session behind it is asleep instead: a forge
    // restart leaves every project in that case, so reading the lifecycle
    // alone would call the whole fleet new.
    const ran: HomeWire = {
      ...wire,
      projects: [
        {
          ...(wire.projects[0] as HomeWire['projects'][0]),
          sessions: [{ last_activity: { secs_since_epoch: 0, nanos_since_epoch: 0 } }],
        },
      ],
    };
    expect(homeView(ran, '').orgs[0]?.projects[0]?.lead.state).toEqual({
      kind: 'lifecycle',
      lifecycle: 'Sleeping',
    });
  });

  it('promotes a backgrounded task to running, and leaves idle alone without one', () => {
    const wire = (has_background_work: boolean): HomeWire => ({
      ...homeWire,
      agents: [{ ...(homeWire.agents[0] as AgentRow), has_background_work }],
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

  it('names the one refusal it can decide, and none it cannot', () => {
    const project = homeWire.projects[0] as HomeWire['projects'][0];
    expect(refusal({ ...project, has_model: false })).toBe(
      'no model declared - add `model` to this project',
    );
    // Whether an account would bind is not in the snapshot, so a project
    // that has a model draws no refusal rather than a guessed one.
    expect(refusal({ ...project, has_model: true })).toBeNull();
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
    const row = { state: { kind: 'lifecycle', lifecycle: 'Idle' }, lastActivity: null } as Row;
    expect(whenOf(row, Date.now())).toBe('now');
  });
});
