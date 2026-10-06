import axe from 'axe-core';
import { JSDOM } from 'jsdom';
import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import Composer from './composer/Composer.svelte';
import type { ComposerProps, ComposerRecord } from './composer/view';
import {
  permissionAsk,
  questionAsk,
  record,
  seatRead,
  slackDraftAsk,
  SLOT,
  take,
  wire,
} from './composer/testing';
import Turn from './chat/Turn.svelte';
import Connect from './connect/Connect.svelte';
import { homeWire } from './dev/fixture.data';
import session from './dev/fixtures/session.json';
import Home from './home/Home.svelte';
import Inspector from './session/Inspector.svelte';
import Session from './session/Session.svelte';
import { sessionFrom, type SessionRecord } from './session/wire';
import type { Connection } from './socket';
import { updateState } from './update/state';
import type { HomeWire } from './wire/home';
import { DEFAULT_SETTINGS } from './wire/types';

/**
 * A connection the server render never reaches: the session page subscribes in
 * an effect, and `render` from `svelte/server` runs none. Every method throws
 * so a page that did reach it fails loudly rather than drawing on nothing.
 */
function untouched(): Connection {
  const refuse = (): never => {
    throw new Error('the server render reached the connection');
  };
  return {
    subscribe: refuse,
    unsubscribe: refuse,
    refresh: refuse,
    dispatch: refuse,
    more: refuse,
    devices: refuse,
    frame: refuse,
    onBrowserAsk: refuse,
    browserRole: refuse,
    onBrowserRole: refuse,
    takeBrowserRole: refuse,
    onMessage: refuse,
    onStatus: refuse,
    store: refuse,
    settings: refuse,
    skew: refuse,
    status: refuse,
    close: refuse,
  };
}

type AxeWindow = Window & typeof globalThis & { axe: typeof axe };

/**
 * What axe finds wrong with a rendered page.
 *
 * **jsdom performs no layout**, so `color-contrast` comes back INCOMPLETE
 * rather than passing or failing. That is why contrast stays a rule in the
 * standard and this function only ever reports violations: a clean result
 * here is not a claim about contrast.
 */
export async function violationsOf(html: string): Promise<axe.Result[]> {
  // The shell carries what `index.html` carries: a language and a title.
  // Without the title axe reports `document-title` on every page, which is a
  // finding about this harness rather than about the page.
  //
  // `runScripts` is required for `window.eval` to run inside the document
  // rather than in the outer context, which is what lets axe see the DOM.
  const dom = new JSDOM(
    `<!doctype html><html lang="en"><head><title>forge</title></head><body>${html}</body></html>`,
    { runScripts: 'dangerously' },
  );
  // One cast, and this is its reason: the object is jsdom's window, whose
  // type cannot carry the `axe` global that the eval below injects into it.
  const window = dom.window as unknown as AxeWindow;
  window.eval(axe.source);
  const results = await window.axe.run(window.document);
  return results.violations;
}

/** Every violation id, which is what a failure should name. */
const idsOf = (html: string) => violationsOf(html).then((found) => found.map((v) => v.id));

/**
 * A button with no accessible name, so the instrument has something to find.
 *
 * **This is the file's positive control.** An empty violations array from a
 * runner that never ran reads exactly like a clean page, and every other test
 * here asserts emptiness.
 */
function PlantedViolation() {
  return '<button></button>';
}

describe('axe over the rendered pages', () => {
  it('catches a violation when one is planted', async () => {
    const html = PlantedViolation();
    expect(await idsOf(html)).toContain('button-name');
  });

  it('draws the home with no violations', async () => {
    const html = render(Home, { props: { wire: homeWire, address: '127.0.0.1:8790' } }).body;
    expect(await idsOf(html)).toEqual([]);
  });

  /**
   * The header's update line is a control, and the plain home draws it only
   * when an update is available - so this is the render that puts it in front
   * of axe. The store is left as it was found: it is shared by every test in
   * this file.
   */
  it('draws the home with an update available, with no violations', async () => {
    updateState.set({ stage: 'available', version: '9.9.9' });
    try {
      const html = render(Home, { props: { wire: homeWire, address: '127.0.0.1:8790' } }).body;
      expect(html, 'the control the update line draws was not rendered').toContain(
        'v9.9.9 available',
      );
      expect(await idsOf(html)).toEqual([]);
    } finally {
      updateState.set({ stage: 'current' });
    }
  });

  it('draws the connect screen with no violations', async () => {
    const html = render(Connect, {
      props: { settings: DEFAULT_SETTINGS, onconnect: () => {} },
    }).body;
    expect(await idsOf(html)).toEqual([]);
  });

  /**
   * The composer's states, because the dock's rows ARE the interaction: a
   * caret was the whole of its selection state until the row carried
   * `aria-selected`, and a caret announces nothing.
   */
  it('draws the composer with no violations', async () => {
    // Inside the landmark its page gives it: the composer is a slot in the
    // session page's own `<main>`, and rendered alone every one of its states
    // reports the page-level `region` rule instead of anything about itself.
    const draw = (held: ComposerRecord) =>
      `<main>${
        render(Composer, {
          props: {
            record: held,
            slot: SLOT,
            seat: seatRead(),
            connection: wire().connection,
            dictation: false,
          } satisfies ComposerProps,
        }).body
      }</main>`;

    expect(await idsOf(draw(record())), 'the box').toEqual([]);
    expect(await idsOf(draw(record({ pending_asks: [permissionAsk()] }))), 'a permission').toEqual(
      [],
    );
    expect(await idsOf(draw(record({ pending_asks: [questionAsk()] }))), 'a question').toEqual([]);
    expect(await idsOf(draw(record({ pending_asks: [slackDraftAsk()] }))), 'a held post').toEqual(
      [],
    );
    expect(
      await idsOf(
        draw(
          record({
            composer: { take: take(), notice: null, compacting: false, sign_in: null },
          }),
        ),
      ),
      'a take recording',
    ).toEqual([]);
    expect(
      await idsOf(
        draw(
          record({
            composer: {
              take: take({ phase: 'transcribing', progress: [2, 6] }),
              notice: null,
              compacting: false,
              sign_in: null,
            },
          }),
        ),
      ),
      'a take transcribing',
    ).toEqual([]);
  });

  it('draws a turn of the conversation with no violations', async () => {
    // A turn is where the chat's interactive elements are: a disclosure per
    // call, with the status mark beside it. The list itself is `virtua`'s and
    // mounts only in a browser, so what is checked here is the markup the
    // chat owns.
    const html = render(Turn, {
      props: {
        turn: {
          key: 't1',
          live: false,
          messages: [
            {
              type: 'user',
              message: { role: 'user', content: [{ type: 'text', text: 'run the gate' }] },
              uuid: 'u1',
            },
            {
              type: 'assistant',
              message: {
                id: 'm1',
                role: 'assistant',
                model: 'claude-opus-5',
                content: [
                  { type: 'text', text: 'running it now' },
                  {
                    type: 'tool_use',
                    id: 'c1',
                    name: 'Bash',
                    input: { command: 'just check' },
                  },
                ],
              },
            },
            {
              type: 'user',
              message: {
                role: 'user',
                content: [{ type: 'tool_result', tool_use_id: 'c1', content: 'all green' }],
              },
              uuid: 'r1',
            },
            // Peer traffic is a disclosure per row now, so the check runs over
            // every row kind the lane draws: an arrival, a send with the ack
            // its result carried, a failure, and the two verb cards.
            {
              type: 'user',
              message: {
                role: 'user',
                content: [
                  {
                    type: 'text',
                    text: "[Message id=m-a11y from agent 'forge/steward' (org 'Busytools')]\n\npicking it up",
                  },
                ],
              },
              uuid: 'p1',
            },
            {
              type: 'assistant',
              message: {
                id: 'm2',
                role: 'assistant',
                model: 'claude-opus-5',
                content: [
                  {
                    type: 'tool_use',
                    id: 's1',
                    name: 'mcp__forge__agents__send_message',
                    input: {
                      org: 'Busytools',
                      project: 'forge',
                      label: 'steward',
                      message: 'the render half is done',
                    },
                  },
                  {
                    type: 'tool_use',
                    id: 's2',
                    name: 'mcp__forge__agents__whoami',
                    input: {},
                  },
                  {
                    type: 'tool_use',
                    id: 's3',
                    name: 'mcp__forge__agents__list',
                    input: {},
                  },
                ],
              },
            },
            {
              type: 'user',
              message: {
                role: 'user',
                content: [
                  {
                    type: 'tool_result',
                    tool_use_id: 's1',
                    content:
                      '{"status":"sent","id":"m-7f3a92e0","to":{"org":"Busytools","project":"forge","label":"steward"}}',
                  },
                  {
                    type: 'tool_result',
                    tool_use_id: 's2',
                    content:
                      '{"name":"forge","org":"Busytools","path":"/tmp/forge","status":"running","slot":{"org":"Busytools","project":"forge","label":"lead"}}',
                  },
                  {
                    type: 'tool_result',
                    tool_use_id: 's3',
                    content:
                      '[{"label":"lead","project":"forge","path":"/tmp/forge","status":"running","slot":{"org":"Busytools","project":"forge","label":"lead"}}]',
                  },
                ],
              },
              uuid: 'r2',
            },
            {
              type: 'user',
              message: {
                role: 'user',
                content: [
                  {
                    type: 'text',
                    text: "[Message to agent 'companies' (org 'Busytools') failed to deliver: channel closed]",
                  },
                ],
              },
              uuid: 'p2',
            },
            // A hook's own run is a disclosure as well - a summary that opens
            // onto the whole of what the hook printed - so the check runs over
            // it too.
            {
              type: 'system',
              subtype: 'hook_started',
              hook_id: 'h1',
              hook_name: 'SessionStart:startup',
              hook_event: 'SessionStart',
              uuid: 'h1',
            },
            {
              type: 'system',
              subtype: 'hook_response',
              hook_id: 'h1',
              hook_name: 'SessionStart:startup',
              hook_event: 'SessionStart',
              output: 'memory index loaded',
              stdout: 'memory index loaded',
              stderr: '',
              exit_code: 0,
              outcome: 'success',
              uuid: 'h2',
            },
          ],
        },
      },
    }).body;
    // Every kind of row carries its own mark, so together they say the rows
    // were really there for the check.
    for (const [mark, what] of [
      ['i-inbox', 'the arrival'],
      ['i-plane', 'the send and the failure'],
      ['i-badge', 'whoami'],
      ['i-users', 'list'],
    ] as const) {
      expect(html, `${what} drew, so axe saw it`).toContain(mark);
    }
    // **What axe audits here is the row CLOSED.** Everything inside a closed
    // `<details>` is out of the tree axe walks, which is the state the row is
    // drawn in until a reader opens it and so the state worth checking - the
    // body is a `<div class="term">` of text today, and an element axe cares
    // about put in there would be checked by nothing until it is opened.
    expect(html, 'and the hook run drew, so axe saw that too').toContain('class="leaf hookrow"');
    // Inside the landmark the session page puts it in: the column is a region
    // of that page rather than a page, and axe reports content outside one.
    expect(await idsOf(`<main>${html}</main>`)).toEqual([]);
  });

  it('draws the session page with no violations', async () => {
    const html = render(Session, {
      props: {
        slot: { org: 'TestOrg', project: 'proj', label: 'lead' },
        connection: untouched(),
        wire: homeWire,
      },
    }).body;
    expect(await idsOf(html)).toEqual([]);
  });

  /**
   * A page with NO record draws eight of its nine sections nowhere, so the
   * case above sees the rail, the header and an empty conversation. This is
   * the one that puts a section body in front of axe - eight of them.
   *
   * The header's facts row and the account chip's popover are NOT in this
   * file's set: the chip lives in the header, and the header is `Session`'s,
   * which no server render can populate. Worth closing when a page can be
   * mounted in a test with a record.
   */
  it('draws the inspector with every section populated, with no violations', async () => {
    const html = render(Inspector, {
      props: {
        wire: populatedHome(),
        record: populated(),
        slot: { org: 'TestOrg', project: 'proj', label: 'lead' },
        now: 1_700_000_000_000,
        onclose: () => {},
      },
    }).body;
    expect(await idsOf(html)).toEqual([]);
  });
});

/** A record with something behind every section the inspector can draw. */
function populated(): SessionRecord {
  const base = sessionFrom(session);
  return {
    ...base,
    mcp: {
      servers: [
        { name: 'forge', status: 'connected', tools: [{}, {}] },
        { name: 'other', status: 'failed', error: 'refused' },
      ],
      error: null,
    },
    processes: {
      scanned_at: { secs_since_epoch: 1_700_000_000, nanos_since_epoch: 0 },
      processes: [
        { pid: 10, parent_pid: 1, name: 'claude', command: 'claude', memory_bytes: 0 },
        { pid: 20, parent_pid: 10, name: 'cargo', command: 'cargo test', memory_bytes: 412 },
      ],
    },
    monitors: [
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
    ],
    conversation: {
      ...base.conversation,
      turns: [
        {
          key: null,
          messages: [
            {
              type: 'assistant',
              message: { content: [{ type: 'tool_use', id: 'tu1', name: 'Task', input: {} }] },
              parent_tool_use_id: null,
            },
          ],
        },
      ],
    },
  };
}

/** A home whose project carries a task, a schedule and both connectors. */
function populatedHome(): HomeWire {
  const project = homeWire.projects[0];
  if (project === undefined) throw new Error('the fixture holds no project');
  return {
    ...homeWire,
    projects: [
      {
        ...project,
        chip: { account_name: 'Acct', state: 'ready' },
        tasks: [
          {
            id: 't1',
            project_name: 'proj',
            subject: 'a task',
            active_form: null,
            detail: null,
            status: 'in_progress',
            owner: { org: 'TestOrg', project: 'proj', label: 'lead' },
            parent: null,
            artifact: null,
            estimate: '2h',
            created_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
            updated_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
          },
        ],
        crons: [
          {
            id: 'c1',
            project_name: 'proj',
            kind: { Recurring: '0 9 * * *' },
            prompt: 'sweep',
            description: 'deps sweep',
            created_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
            next_fire: { secs_since_epoch: 1_700_003_600, nanos_since_epoch: 0 },
          },
        ],
        connectors: {
          gotify: [{ applications: ['homelab'], min_priority: 4 }],
          slack: [
            {
              id: 's1',
              workspace: 'Trust Machines',
              target: { Conversation: { id: 'C1', name: '#alerts', mode: 'All' } },
            },
            { id: 's2', workspace: 'Trust Machines', target: 'Mentions' },
          ],
        },
      },
    ],
    connectors: {
      gotify: { connected: true },
      slack: {
        connected_workspaces: [['Trust Machines', true]],
        load_failed: false,
      },
    },
  };
}
