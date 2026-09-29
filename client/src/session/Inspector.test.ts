import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import session from '../dev/fixtures/session.json';
import type { HomeWire } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import Inspector from './Inspector.svelte';
import { sessionFrom, type SessionRecord } from './wire';

const record: SessionRecord = sessionFrom(session);
const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };
const NOW = 1_700_000_000_000;

const draw = (props: { record?: SessionRecord | null; wire?: HomeWire } = {}): string =>
  render(Inspector, {
    props: {
      wire: props.wire ?? homeWire,
      record: props.record === undefined ? record : props.record,
      slot: LEAD,
      now: NOW,
      onclose: () => {},
    },
  }).body;

/** The `data-k` of every section the inspector drew, in the order it drew them. */
function sections(body: string): string[] {
  return [...body.matchAll(/data-k="sec-([a-z ]+)"/g)].map((match) => match[1] ?? '');
}

const withMcp = (servers: SessionRecord['mcp']): SessionRecord => ({ ...record, mcp: servers });
const withWork = (work: SessionRecord['work'], pr: SessionRecord['pr'] = null): SessionRecord => ({
  ...record,
  work,
  pr,
});

describe('the inspector as it draws', () => {
  /**
   * The server's own rule: a section with nothing behind it is not drawn, and
   * it is why a fresh seat's page looks short. The fixture's session has an
   * empty conversation, no monitors and no MCP read.
   */
  it('draws only the sections that have something behind them', () => {
    expect(sections(draw())).toEqual(['git']);
  });

  /**
   * The git section leads because the working tree is the first thing a person
   * looks for, and because it is the one read whose absence used to read as
   * "this seat has no repository".
   */
  it('leads with git, above every other section', () => {
    const drawn = sections(
      draw({
        record: withMcp({
          servers: [{ name: 'forge', status: 'connected', tools: [] }],
          error: null,
        }),
      }),
    );
    expect(drawn[0]).toBe('git');
    expect(drawn).toContain('mcp servers');
  });

  it('draws the branch and the pull request the record carries', () => {
    const body = draw({
      record: withWork(
        { branch: 'web-home-layout', changed: 8, gate: 'in_repo' },
        { number: 1203, url: '' },
      ),
    });
    expect(body).toContain('web-home-layout \u{b7} 8 files');
    expect(body).toContain('PR #1203');
    expect(body).toContain('closes #1215');
  });

  /**
   * A section that is always there says nothing when it is empty - and the
   * subagents section is the one carry whose absence would read as "none ran",
   * so it is drawn against the conversation rather than against the catalogue.
   */
  it('draws the subagents gap only when the conversation shows a dispatch', () => {
    expect(sections(draw())).not.toContain('subagents');
    const dispatched: SessionRecord = {
      ...record,
      conversation: {
        ...record.conversation,
        messages: [
          {
            type: 'assistant',
            message: { content: [{ type: 'tool_use', id: 'tu1', name: 'Task', input: {} }] },
            parent_tool_use_id: null,
          },
        ],
      },
    };
    expect(sections(draw({ record: dispatched }))).toContain('subagents');
  });

  it('draws the monitors section with what a card is watching', () => {
    const body = draw({
      record: {
        ...record,
        monitors: [
          {
            tool_use_id: 'm1',
            task_id: null,
            description: 'ci-watch',
            command: 'gh run watch 18234567',
            persistent: true,
            timeout_ms: 0,
            status: 'running',
            output_file: null,
            ended_at: null,
          },
        ],
      },
    });
    expect(sections(body)).toContain('monitors');
    expect(body).toContain('ci-watch');
    expect(body).toContain('persistent');
    expect(body).toContain('gh run watch 18234567');
  });

  /**
   * The processes section is the one that came from a character grid: the
   * terminal indented each row with two spaces per level, and a list inside a
   * list is the shape it was drawing.
   */
  it('nests a process under its parent rather than indenting it with spaces', () => {
    const body = draw({
      record: {
        ...record,
        processes: {
          scanned_at: { secs_since_epoch: 1_700_000_000, nanos_since_epoch: 0 },
          processes: [
            { pid: 10, parent_pid: 1, name: 'claude', command: 'claude', memory_bytes: 0 },
            {
              pid: 20,
              parent_pid: 10,
              name: 'cargo',
              command: '/opt/homebrew/bin/cargo nextest run',
              memory_bytes: 412 * 1024 * 1024,
            },
          ],
        },
      },
    });
    expect(sections(body)).toContain('processes');
    expect(body).toContain('cargo nextest run');
    expect(body).toContain('412 MB');
    // The child sits in the parent's own item, which is what makes it a level
    // of hierarchy rather than a row drawn after it.
    expect(body, 'the child was drawn beside its parent rather than under it').toMatch(
      /<li>[\s\S]*claude[\s\S]*<ul class="tree">[\s\S]*cargo nextest run/,
    );
    // A non-breaking space is the character run the terminal indented with.
    expect(body, 'a character run stood in for the nesting').not.toContain('\u{a0}');
  });

  it('draws an empty MCP read as the failure it is, with the reason', () => {
    const body = draw({ record: withMcp({ servers: [], error: 'the CLI refused' }) });
    expect(body).toContain('failed');
    expect(body).toContain('the CLI refused');
  });

  it('hangs each slack subscription off the workspace it watches', () => {
    const project = homeWire.projects[0];
    if (project === undefined) throw new Error('the fixture holds no project');
    const body = draw({
      wire: {
        ...homeWire,
        connectors: {
          gotify: { connected: false, subscriptions: [] },
          slack: {
            connected_workspaces: [['Trust Machines', true]],
            load_failed: false,
            subscriptions: [
              {
                workspace: 'Trust Machines',
                target: { Conversation: { id: 'C1', name: '#granite-alerts', mode: 'All' } },
              },
            ],
          },
        },
      },
    });
    expect(sections(body)).toContain('slack');
    expect(body).toMatch(/<li>[\s\S]*Trust Machines[\s\S]*<ul class="subs">[\s\S]*#granite-alerts/);
  });

  it('draws the tasks a project holds, in the order a person reads them', () => {
    const project = homeWire.projects[0];
    if (project === undefined) throw new Error('the fixture holds no project');
    const task = {
      id: 't1',
      project_name: 'proj',
      subject: 'Land the Claude version on the core',
      active_form: null,
      detail: null,
      status: 'in_progress' as const,
      owner: LEAD,
      parent: null,
      artifact: null,
      estimate: null,
      created_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
      updated_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
    };
    const body = draw({ wire: { ...homeWire, projects: [{ ...project, tasks: [task] }] } });
    expect(sections(body)).toContain('tasks');
    expect(body).toContain('Land the Claude version on the core');
    expect(body).toContain('class="tk now"');
  });

  it('draws no section at all for a record that has not landed', () => {
    expect(sections(draw({ record: null }))).toEqual([]);
    expect(draw({ record: null })).toContain('close the inspector');
  });
});
