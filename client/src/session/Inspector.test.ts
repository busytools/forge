// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import session from '../dev/fixtures/session.json';
import type { HomeWire } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import Inspector from './Inspector.svelte';
import { sessionFrom, type SessionRecord } from './wire';

const record: SessionRecord = sessionFrom(session);
const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };
const NOW = 1_700_000_000_000;

let app: Record<string, unknown> | null = null;
let host: HTMLElement | null = null;

/** What the inspector is drawing right now, which is what an opening changes. */
function drawn(): string {
  return host?.innerHTML ?? '';
}

/**
 * The inspector, mounted, as the markup it drew.
 *
 * **Mounted rather than server-rendered, because a section's body is drawn
 * when it is open.** A server render has no way to open one, so it can only
 * ever see the closed shape: names, summaries and nothing behind them.
 */
function draw(props: { record?: SessionRecord | null; wire?: HomeWire } = {}): string {
  if (app !== null) void unmount(app);
  document.body.innerHTML = '';
  host = document.createElement('div');
  document.body.append(host);
  app = mount(Inspector, {
    target: host,
    props: {
      wire: props.wire ?? homeWire,
      record: props.record === undefined ? record : props.record,
      slot: LEAD,
      now: NOW,
      onclose: () => {},
    },
  });
  flushSync();
  return host.innerHTML;
}

/**
 * Open a section, which is what a reader does before its body means anything.
 *
 * jsdom does not implement `<summary>` activation, so a `click` would toggle
 * nothing here while looking like it had; this is the property-and-event pair
 * `bind:open` actually listens for.
 */
function openSection(name: string): void {
  const found = document.querySelector(`details.sec[data-k="sec-${name}"]`);
  if (!(found instanceof HTMLDetailsElement)) throw new Error(`no ${name} section was drawn`);
  found.open = true;
  found.dispatchEvent(new Event('toggle'));
  flushSync();
}

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

/** The `data-k` of every section the inspector drew, in the order it drew them. */
function sections(body: string): string[] {
  return [...body.matchAll(/data-k="sec-([a-z ]+)"/g)].map((match) => match[1] ?? '');
}

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
      }),
    );
    expect(drawn[0]).toBe('git');
    expect(drawn).toContain('monitors');
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

  it('draws the monitors section with what a card is watching', () => {
    draw({
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
    openSection('monitors');
    const body = drawn();
    expect(sections(body)).toContain('monitors');
    expect(body).toContain('ci-watch');
    expect(body).toContain('persistent');
    expect(body).toContain('gh run watch 18234567');
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
    draw({ wire: { ...homeWire, projects: [{ ...project, tasks: [task] }] } });
    openSection('tasks');
    const body = drawn();
    expect(sections(body)).toContain('tasks');
    expect(body).toContain('Land the Claude version on the core');
    expect(body).toContain('class="tk now"');
  });

  it('draws no section at all for a record that has not landed', () => {
    expect(sections(draw({ record: null }))).toEqual([]);
    expect(draw({ record: null })).toContain('close the inspector');
  });
});
