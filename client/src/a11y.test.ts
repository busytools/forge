import axe from 'axe-core';
import { JSDOM } from 'jsdom';
import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import Pinned from './chat/Pinned.svelte';
import { connectors } from './chat/connectors.svelte';
import { git } from './chat/git.svelte';
import { mcp } from './chat/mcp.svelte';
import { monitors } from './chat/monitors.svelte';
import { processes } from './chat/processes.svelte';
import { schedules } from './chat/schedules.svelte';
import { subagents } from './chat/subagents.svelte';
import { tasks } from './chat/tasks.svelte';
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
import Home from './home/Home.svelte';
import Models from './models/Models.svelte';
import ModelsBody from './models/ModelsBody.svelte';
import { modelsWire } from './models/testing';
import Palette from './session/Palette.svelte';
import Session from './session/Session.svelte';
import { PROTOCOL_VERSION } from './protocol';
import type { Connection } from './socket';
import { updateState } from './update/state';
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
    // The rail's footer reads the protocol pair as it RENDERS, so this one
    // answers: refusing it would be refusing the page, not the socket.
    serverProtocol: () => PROTOCOL_VERSION,
    status: refuse,
    close: refuse,
  };
}

/**
 * A connection the strip's own segments may read: the browser segment
 * registers a role listener and reads the role as it draws, so this answers
 * those and refuses nothing - the check never presses Take over.
 */
function browserIdle(): Connection {
  return {
    browserRole: () => false,
    onBrowserRole: () => () => {},
    takeBrowserRole: () => {},
  } as unknown as Connection;
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

  /**
   * The stopped-forge state (#1638): the home's one fatal row, in front of
   * axe like every other state a page can be in.
   */
  it('draws the stopped-forge home with no violations', async () => {
    const html = render(Home, {
      props: {
        wire: {
          ...homeWire,
          fatal_error: 'Failed to establish or maintain the Agent SDK bridge connection.',
        },
        address: '127.0.0.1:8790',
      },
    }).body;
    expect(html, 'the fatal row was not rendered').toContain('forge stopped:');
    expect(await idsOf(html)).toEqual([]);
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
   * The strip with every row populated, which is where the inspector's
   * sections live now. Rendered with NO turn pinned, which is also the shape
   * an idle seat draws - the case the strip had no rendering for at all
   * before it outlived the turn.
   *
   * The header's facts row and the account chip's popover are NOT in this
   * file's set: the chip lives in the header, and the header is `Session`'s,
   * which no server render can populate. Worth closing when a page can be
   * mounted in a test with a record.
   */
  it('draws the strip with every row populated, with no violations', async () => {
    seedStrip(true);
    try {
      const html = render(Pinned, { props: { info: null, connection: browserIdle() } }).body;
      // Every row drew, so axe saw all of them.
      for (const [mark, what] of [
        ['i-subagents', 'the agents row'],
        ['i-processes', 'the processes row'],
        ['i-gotify', 'the connectors row'],
        ['i-schedules', 'the schedules row'],
        ['i-mcp', 'the servers row'],
        ['i-git', 'the tree row'],
        ['i-tasks', 'the tasks row'],
        ['i-monitors', 'the monitors row'],
      ] as const) {
        expect(html, `${what} drew, so axe saw it`).toContain(mark);
      }
      expect(await idsOf(`<main>${html}</main>`)).toEqual([]);
    } finally {
      seedStrip(false);
    }
  });

  /**
   * **The palette's open dialog**, which no other case renders: the session
   * page draws it closed, so its combobox roles, the listbox's options and
   * the dialog's name would otherwise go unguarded.
   */
  it('draws the open command palette with no violations', async () => {
    const body = render(Palette, {
      props: {
        open: true,
        wire: homeWire,
        slot: SLOT,
        connection: untouched(),
        sessionId: 'd4f70669-1f2a-4c88',
        onclose: () => undefined,
        onpeek: () => undefined,
      },
    }).body;
    expect(await violationsOf(body)).toEqual([]);
  });

  it('draws the models page with no violations', async () => {
    const html = render(ModelsBody, {
      props: {
        wire: modelsWire,
        oncheck: () => {},
        oninstall: () => {},
        onactivate: () => {},
        ondeactivate: () => {},
        onbench: () => {},
        onbenchstop: () => {},
        onrecord: () => {},
        onrecordstop: () => {},
        onrecorddelete: () => {},
        onbenchdelete: () => {},
        onupdate: () => {},
        onsweep: () => {},
        onsweepcancel: () => {},
        onadopt: () => {},
        onuninstall: () => {},
      },
    }).body;
    expect(html, 'the feed drew, so axe saw it').toContain('update available');
    expect(await idsOf(html)).toEqual([]);
  });

  /**
   * **The states this page gained, in front of axe**: a sweep's verdicts with
   * their switches, a bench that failed with its close, and the read-aloud set
   * being recorded. None of them is on the ordinary render, so without a case
   * of their own their labels and controls would go unguarded.
   */
  it('draws a verdict, a failed bench and a recording with no violations', async () => {
    const html = render(ModelsBody, {
      props: {
        wire: {
          ...modelsWire,
          bench: {
            state: 'failed',
            target: { file: 'a-norm-a.gguf', role: 'transcribing', pinned: false },
            reason: 'No such file or directory (os error 2)',
          },
          read_aloud: {
            recordings: [],
            recording: true,
            error: null,
            passage: 'the passage',
            terms: ['forge'],
            unknown: false,
          },
        },
        verdicts: [
          {
            role: 'cleanup',
            best: {
              run: {
                variant: 'a/norm-a',
                role: 'cleanup',
                file: 'a-norm-a-Q4_K_M.gguf',
                size_bytes: 1,
                installed: false,
                why: 'candidate',
              },
              result: {
                target: { file: 'a-norm-a-Q4_K_M.gguf', role: 'cleanup', pinned: false },
                tier: 'read_aloud',
                metrics: {
                  clips: 1,
                  audio_seconds: 12,
                  wall_seconds: 3,
                  xrt_wall: 4,
                  term_accuracy: 0.9,
                  wer: 0.1,
                  matched: null,
                  stages_ms: {
                    model_load_ms: 1,
                    resample_ms: 1,
                    mel_ms: 1,
                    encode_ms: 1,
                    decode_ms: 1,
                    normalize_ms: 1,
                  },
                },
                at: '2026-10-08T00:00:00Z',
                corpus: { clips: 1, audio_seconds: 12, sha256: 'aa' },
              },
            },
            baseline: null,
            onBest: false,
            scored: 1,
            beyond: 4,
            tried: 3,
            pick: false,
            tier: 'read_aloud',
          },
        ],
        oncheck: () => {},
        oninstall: () => {},
        onactivate: () => {},
        ondeactivate: () => {},
        onbench: () => {},
        onbenchstop: () => {},
        onrecord: () => {},
        onrecordstop: () => {},
        onrecorddelete: () => {},
        onbenchdelete: () => {},
        onupdate: () => {},
        onsweep: () => {},
        onsweepcancel: () => {},
        onadopt: () => {},
        onuninstall: () => {},
      },
    }).body;
    expect(html, 'the verdict drew, so axe saw it').toContain('switch to it');
    expect(html).toContain('the bench did not finish');
    expect(html).toContain('the read-aloud set is being recorded');
    expect(await idsOf(html)).toEqual([]);
  });

  /**
   * The page with a download running: the progress line, its bar and the
   * disabled controls a second press would land on.
   */
  it('draws a download in flight with no violations', async () => {
    const html = render(ModelsBody, {
      props: {
        wire: {
          ...modelsWire,
          install: {
            state: 'downloading',
            file: 'granite-speech-5.0-470m-turboctc-Q4_K_M.gguf',
            got: 106_000_000,
            total: 279_000_000,
          },
        },
        oncheck: () => {},
        oninstall: () => {},
        onactivate: () => {},
        ondeactivate: () => {},
        onbench: () => {},
        onbenchstop: () => {},
        onrecord: () => {},
        onrecordstop: () => {},
        onrecorddelete: () => {},
        onbenchdelete: () => {},
        onupdate: () => {},
        onsweep: () => {},
        onsweepcancel: () => {},
        onadopt: () => {},
        onuninstall: () => {},
      },
    }).body;
    expect(html).toContain('downloading');
    expect(await idsOf(html)).toEqual([]);
  });

  /**
   * Dictation off: no pins, no check, and no rows - the catalogue is read
   * only while the section is on, so the search's own off state draws there.
   */
  it('draws the models page with dictation off, with no violations', async () => {
    const html = render(ModelsBody, {
      props: {
        wire: {
          ...modelsWire,
          enabled: false,
          in_use: [],
          updates: [],
          check: { state: 'never' },
          rows: [],
        },
        oncheck: () => {},
        oninstall: () => {},
        onactivate: () => {},
        ondeactivate: () => {},
        onbench: () => {},
        onbenchstop: () => {},
        onrecord: () => {},
        onrecordstop: () => {},
        onrecorddelete: () => {},
        onbenchdelete: () => {},
        onupdate: () => {},
        onsweep: () => {},
        onsweepcancel: () => {},
        onadopt: () => {},
        onuninstall: () => {},
      },
    }).body;
    expect(html).toContain('dictation is off');
    expect(html).toContain('read only with');
    expect(await idsOf(html)).toEqual([]);
  });

  /** The route before the first read lands: the page's own loading state. */
  it('draws the models route waiting for its read, with no violations', async () => {
    const html = render(Models, { props: { connection: null } }).body;
    expect(html).toContain('Reading the models');
    expect(await idsOf(html)).toEqual([]);
  });
});

/**
 * Every strip row's store, seeded (or cleared) as one.
 *
 * The strip reads the module singletons rather than props, and this file
 * shares them with the checks around it, so the clear is as much the fixture
 * as the seed.
 */
function seedStrip(on: boolean): void {
  subagents.sync(
    on
      ? [
          {
            name: 'map the calls',
            dispatch_id: 'tu-sub',
            agent_type: 'general-purpose',
            running: true,
            failed: false,
            backgrounded: false,
            ended_at: null,
            calls: 2,
            tail: [],
            usage: null,
          },
        ]
      : null,
  );
  processes.sync(
    on
      ? {
          scanned_at: { secs_since_epoch: 1_700_000_000, nanos_since_epoch: 0 },
          processes: [
            { pid: 10, parent_pid: 1, name: 'cargo', command: 'cargo test', memory_bytes: 412 },
          ],
        }
      : null,
    on
      ? [
          {
            task_id: 't-1',
            task_type: 'local_bash',
            description: 'run the gate',
            command: 'cargo test',
            tool_use_id: 'tu-bash',
          },
        ]
      : null,
    false,
    { calls: new Map(), owners: new Map() },
  );
  connectors.sync(on ? [{ kind: 'gotify', id: 'g-1', key: 'ci', value: 'any priority' }] : null);
  schedules.sync(on ? [{ id: 'c-1', key: 'rules sweep', value: 'in 27d \u{b7} recurring' }] : null);
  mcp.sync(
    on
      ? [
          {
            name: 'forge',
            k: 'forge \u{b7} session',
            v: '2 tools',
            tools: ['roster'],
            command: 'node forge-server.js',
            reason: null,
            synthetic: false,
          },
        ]
      : null,
  );
  git.sync(
    on
      ? {
          label: 'feat/x \u{b7} 3 files',
          head: "the project's tree",
          ahead: null,
          uncommitted: {
            files: [{ path: 'a.rs', added: 1, removed: 0, status: 'modified' as const }],
            totalFiles: 1,
            totalAdded: 1,
            totalRemoved: 0,
          },
          pr: null,
          gate: null,
        }
      : null,
  );
  tasks.sync(
    on
      ? [
          {
            id: 't1',
            status: 'in_progress',
            display: 'doing a task',
            subject: 'a task',
            owner: 'lead',
            meta: 'in progress \u{b7} 2h',
          },
        ]
      : null,
  );
  monitors.sync(
    on
      ? [
          {
            id: 'm1',
            running: true,
            completed: false,
            name: 'ci-watch',
            label: 'persistent',
            command: 'gh run watch 18234567',
          },
        ]
      : null,
  );
}
