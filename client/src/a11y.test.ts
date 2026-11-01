import { JSDOM } from 'jsdom';
import { render } from 'svelte/server';
import { describe, expect, it, vi } from 'vitest';

import Board from './board/Board.svelte';
import { connectors } from './chat/connectors.svelte';
import { files } from './chat/files.svelte';
import { git } from './chat/git.svelte';
import { mcp } from './chat/mcp.svelte';
import { monitors } from './chat/monitors.svelte';
import { processes } from './chat/processes.svelte';
import { schedules } from './chat/schedules.svelte';
import { subagents } from './chat/subagents.svelte';
import { tasks } from './chat/tasks.svelte';
import Composer from './composer/Composer.svelte';
import LiveRow from './composer/LiveRow.svelte';
import type { ComposerProps, ComposerRecord } from './composer/view';
import { surface } from './browser/surface.svelte';
import type { FileEntry } from './composer/wire';
import FileTree from './components/FileTree.svelte';
import type { TreeRow } from './components/tree';
import { fileIcon, folderIcon, rootIcon } from './files/icons';
import { foldsFor, indexTreeRows } from './files/tree';
import {
  browserHandOffAsk,
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
import { boardWire, homeWire } from './dev/fixture.data';
import type { BoardWire } from './wire/board';
import Document from './doc/Document.svelte';
import Files from './files/Files.svelte';
import ExtensionsBody from './extensions/ExtensionsBody.svelte';
import { extensionsWire, project, recorder } from './extensions/testing';
import SettingsBody from './settings/SettingsBody.svelte';
import { settingsWire } from './dev/settings.fixture';
import Home from './home/Home.svelte';
import Models from './models/Models.svelte';
import Review from './review/Review.svelte';
import ModelsBody from './models/ModelsBody.svelte';
import { modelsWire } from './models/testing';
import Palette from './session/Palette.svelte';
import Session from './session/Session.svelte';
import { PROTOCOL_VERSION } from './protocol';
import type { Connection } from './socket';
import { violationsOf } from './testing/axe';
import { updateState } from './update/state';
import { DEFAULT_SETTINGS } from './wire/types';

/**
 * The browser's link, as the surface's own gate: a page draws the takeover
 * only when the link says it can, and a server render installs none. The
 * open-takeover render below needs that gate open, and nothing else here
 * reaches the link.
 */
vi.mock('./browser/link', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./browser/link')>();
  return { ...actual, drawsSurface: (): boolean => true };
});

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
    showing: refuse,
    relist: refuse,
    declare: refuse,
    release: refuse,
    dispatch: refuse,
    more: refuse,
    commitDiff: refuse,
    fileContent: refuse,
    readDocument: refuse,
    seatRange: refuse,
    frame: refuse,
    browserInput: refuse,
    onBrowserFrame: refuse,
    onMessage: refuse,
    onStatus: refuse,
    store: refuse,
    settings: refuse,
    skew: refuse,
    // The rail's footer reads the protocol pair as it RENDERS, so this one
    // answers: refusing it would be refusing the page, not the socket.
    serverProtocol: () => PROTOCOL_VERSION,
    status: refuse,
    probe: refuse,
    close: refuse,
  };
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

  it('draws a project board with no violations', async () => {
    // The fixture's board is empty, which is one of its states; a row that
    // waits on the reader is the other state the page draws controls in.
    const first = boardWire.projects[0];
    if (first === undefined) throw new Error('the fixture holds no project');
    const withWaiting: BoardWire = {
      projects: [
        {
          ...first,
          rows: [
            {
              task: {
                id: 't1',
                project_name: 'proj',
                subject: 'a row waiting on the reader',
                active_form: null,
                detail: null,
                status: 'waiting' as const,
                owner: null,
                parent: null,
                waiting_on: {
                  kind: 'decision' as const,
                  detail: 'the mockup',
                  on: null,
                  verification: true,
                },
                estimate: null,
                rank: null,
                verify: 'user' as const,
                // A look with a reference and an artifact, so the card's new
                // link and door markup is under axe rather than absent.
                links: [
                  {
                    kind: 'pr' as const,
                    label: 'PR #2425',
                    target: 'https://example.invalid/pull/2425',
                    state: null,
                    added_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
                  },
                  {
                    kind: 'path' as const,
                    label: 'the board page',
                    target: 'https://example.invalid/board',
                    state: null,
                    added_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
                  },
                ],
                attempt: 1,
                archived_at: null,
                created_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
                updated_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
              },
              worked_secs: 300,
              updated_secs_ago: 60,
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
              history: [],
              history_total: 0,
            },
          ],
        },
      ],
    };
    const html = render(Board, {
      props: {
        wire: homeWire,
        board: { wire: withWaiting, refused: null },
        org: 'TestOrg',
        project: 'proj',
      },
    }).body;
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
    const draw = (held: ComposerRecord, dictation = false) =>
      `<main>${
        render(Composer, {
          props: {
            record: held,
            slot: SLOT,
            seat: seatRead(),
            connection: wire().connection,
            dictation,
            voice: false,
            reading: false,
          } satisfies ComposerProps,
        }).body
      }</main>`;

    expect(await idsOf(draw(record())), 'the box').toEqual([]);
    expect(await idsOf(draw(record(), true)), 'the box with the mic').toEqual([]);
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
            composer: { take: take(), notice: null, machine: null },
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
              machine: null,
            },
          }),
        ),
      ),
      'a take transcribing',
    ).toEqual([]);
  });

  /**
   * **The takeover's bar draws over the composer**, and the closed render
   * never reached it: a regression in the bar's own semantics was unpinned
   * (#2571). **The bar's own names are asserted**, and the role is held with
   * a tab list so the arrows and the picker are in the audited markup too,
   * not only the bar's empty state. The surface state is left as it was
   * found, the way the store's other case in this file leaves it.
   */
  it('draws the open browser takeover with no violations', async () => {
    const was = {
      phase: surface.phase,
      profile: surface.profile,
      tab: surface.tab,
      seat: surface.seat,
      viewer: surface.viewer,
      controller: surface.controller,
      nav: surface.nav,
      tabs: surface.tabs,
    };
    surface.phase = 'live';
    surface.profile = 'Busytools/default';
    surface.tab = 2;
    surface.seat = SLOT;
    surface.viewer = 7;
    surface.controller = 7;
    surface.nav = { can_back: true, can_forward: true };
    surface.tabs = {
      profile: 'Busytools/default',
      id: 1,
      up: true,
      connected: true,
      closing_at_ms: null,
      rows: [
        {
          number: 2,
          title: 'the sign-in page',
          url: 'https://example.com/',
          owner: SLOT,
          state: 'held',
        },
        {
          number: 3,
          title: '',
          url: 'https://example.org/',
          owner: SLOT,
          state: 'idle',
        },
      ],
    };
    try {
      const html = `<main>${
        render(Composer, {
          props: {
            record: record({ pending_asks: [browserHandOffAsk()] }),
            slot: SLOT,
            seat: seatRead(),
            connection: wire().connection,
            dictation: false,
            voice: false,
            reading: false,
          } satisfies ComposerProps,
        }).body
      }</main>`;
      // **The server render is where the composer's own inert is proved**:
      // the bar's mount walk covers it in a browser, so the mounted test
      // cannot tell the attribute from the walk.
      expect(html, 'the composer behind the picture is inert').toMatch(
        /<div class="surf[^"]*"[^>]*\binert\b/,
      );
      for (const name of [
        'aria-label="the browser takeover"',
        'aria-label="back"',
        'aria-label="forward"',
        'aria-label="the tab to view"',
        'back to chat',
        'release control',
        'Done',
        'Not now',
        "something's wrong\u{2026}",
      ]) {
        expect(html, `the bar draws ${name}`).toContain(name);
      }
      expect(await idsOf(html)).toEqual([]);
    } finally {
      surface.phase = was.phase;
      surface.profile = was.profile;
      surface.tab = was.tab;
      surface.seat = was.seat;
      surface.viewer = was.viewer;
      surface.controller = was.controller;
      surface.nav = was.nav;
      surface.tabs = was.tabs;
    }
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
              hook_name: 'PostToolUse:Edit',
              hook_event: 'PostToolUse',
              uuid: 'h1',
            },
            {
              type: 'system',
              subtype: 'hook_response',
              hook_id: 'h1',
              hook_name: 'PostToolUse:Edit',
              hook_event: 'PostToolUse',
              output: 'format check failed',
              stdout: 'format check failed',
              stderr: '',
              exit_code: 2,
              outcome: 'error',
              uuid: 'h2',
            },
            // And a session start's row, which is the disclosure those hooks
            // gather under: a summary of its own, opening onto them.
            {
              type: 'system',
              subtype: 'hook_started',
              hook_id: 'h3',
              hook_name: 'SessionStart:startup',
              hook_event: 'SessionStart',
              uuid: 'h3',
            },
            {
              type: 'system',
              subtype: 'hook_response',
              hook_id: 'h3',
              hook_name: 'SessionStart:startup',
              hook_event: 'SessionStart',
              output: 'memory index loaded',
              stdout: 'memory index loaded',
              stderr: '',
              exit_code: 0,
              outcome: 'success',
              uuid: 'h4',
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
    expect(html, "and the session start's row drew").toContain('class="leaf hookgroup"');
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
   * The review page before its read lands: the state a server render draws,
   * the same way the session page's is drawn above. Its populated states are
   * the subscription's, which a server render does not run.
   */
  it('draws the review page with no violations', async () => {
    const html = render(Review, {
      props: {
        slot: { org: 'TestOrg', project: 'proj', label: 'lead' },
        connection: untouched(),
        dictation: false,
      },
    }).body;
    expect(await idsOf(html)).toEqual([]);
  });

  /**
   * The files page before its read lands, the review page's own shape: the
   * populated states are the subscription's, which a server render does not
   * run.
   */
  it('draws the files page with no violations', async () => {
    const html = render(Files, {
      props: {
        slot: { org: 'TestOrg', project: 'proj', label: 'lead' },
        connection: untouched(),
        dictation: false,
      },
    }).body;
    expect(await idsOf(html)).toEqual([]);
  });

  /**
   * The folded tree, which is the first state a screen reader meets: a
   * folder holding a file says it is closed, a folder holding only folders
   * says it is open, and the keyboard's row keeps its mark through a fold.
   */
  it('draws the folded tree with its closed rows named, with no violations', async () => {
    const entries: FileEntry[] = [
      'client/src/files/Files.svelte',
      'client/package.json',
      'crates/forge-server/src/surface.rs',
      'README.md',
    ].map((relPath) => ({
      relPath,
      relPathLower: relPath.toLowerCase(),
      basenameLower: (relPath.split('/').pop() ?? relPath).toLowerCase(),
      depth: relPath.split('/').length - 1,
    }));
    const closed = foldsFor(entries, '', new Map());
    const draw = (shut: ReadonlySet<string>, foldable = true): string =>
      render(FileTree, {
        props: {
          rows: indexTreeRows(entries, shut),
          heading: 'Tree',
          label: "the seat's tree",
          count: entries.length,
          closed: shut,
          here: null,
          active: 'client',
          foldable,
          icon: (row: TreeRow<unknown>) =>
            row.kind === 'file' ? fileIcon(row.path) : folderIcon(row.path, true),
          root: { name: 'proj', path: '/proj/forge', icon: rootIcon('/proj/forge') },
          ondir: () => {},
          onfile: () => {},
        },
      }).body;
    const html = draw(closed);
    const doc = new JSDOM(html).window.document;
    const row = (path: string): Element | null => doc.querySelector(`[data-path="${path}"]`);
    expect(
      row('client')?.getAttribute('aria-expanded'),
      'a folder holding a file starts shut',
    ).toBe('false');
    expect(
      row('crates')?.getAttribute('aria-expanded'),
      'a folder holding only folders starts open',
    ).toBe('true');
    expect(
      row('client')?.classList.contains('key'),
      'the keyboard mark is on the active row, not any row',
    ).toBe(true);
    expect(
      row('client')?.querySelector('img.rv-icon')?.getAttribute('alt'),
      'glyphs are decorative',
    ).toBe('');
    const root = doc.querySelector('.rv-root');
    expect(root?.querySelector('.rv-rootname')?.textContent, 'the root row draws the project').toBe(
      'proj',
    );
    expect(
      root?.querySelector('img.rv-icon')?.getAttribute('alt'),
      'the root glyph is decorative too',
    ).toBe('');
    expect(
      (root?.querySelector('img.rv-icon')?.getAttribute('src') ?? '').split('?')[0],
      'the root glyph is the theme root folder',
    ).toMatch(/\/folder-root-open\.svg$/);
    expect(await idsOf(html)).toEqual([]);

    // Under a filter the folder rows are a match list's, not controls.
    const filtered = new JSDOM(draw(foldsFor(entries, 'surface', new Map()), false)).window
      .document;
    const plain = filtered.querySelector('[data-path="crates"]');
    expect(plain?.tagName, 'a filtered folder row is not a control').toBe('DIV');
    expect(plain?.hasAttribute('aria-expanded'), 'and announces no disclosure').toBe(false);
    expect(plain?.querySelector('.rv-arw')?.textContent, 'and draws no chevron').toBe('');
    expect(await idsOf(filtered.body.innerHTML)).toEqual([]);
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
  /**
   * **The live row** is the strip's successor, and it draws the same segments
   * as a list of doors. Rendered with NO turn, which is also the shape an idle
   * seat draws - the case the row would otherwise have no rendering for.
   */
  it('draws the live row with every chip, with no violations', async () => {
    seedStrip(true);
    try {
      const html = render(LiveRow, { props: { info: null, slot: SLOT } }).body;
      for (const [mark, what] of [
        ['i-subagents', 'the agents chip'],
        ['i-processes', 'the processes chip'],
        ['i-gotify', 'the connectors chip'],
        ['i-schedules', 'the schedules chip'],
        ['i-mcp', 'the servers chip'],
        ['i-git', 'the tree chip'],
        ['i-tasks', 'the tasks chip'],
        ['i-monitors', 'the monitors chip'],
      ] as const) {
        expect(html, `${what} drew, so axe saw it`).toContain(mark);
      }
      const page = new JSDOM(`<main>${html}</main>`).window.document;
      const list = page.querySelector('[role="list"]');
      expect(list, 'the chips are not a list').not.toBeNull();
      for (const item of page.querySelectorAll('[data-chip]')) {
        if (item.firstElementChild === null) continue;
        // A chip is a door: a button that opens its panel, or a real link
        // where the segment's own read lives on a page.
        const door = item.querySelector('button, a');
        expect(door, 'a drawn chip carries no door').not.toBeNull();
        const name = (door?.getAttribute('aria-label') ?? door?.textContent ?? '').trim();
        expect(name, 'a drawn chip has no readable name').not.toBe('');
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

  /**
   * The document viewer, as a server render reaches it: the read is in an
   * effect, so this is the state the page opens in - the pending line under
   * its own `<main>` landmark.
   */
  it('draws the document viewer opening, with no violations', async () => {
    const html = render(Document, {
      props: {
        slot: { org: 'Busytools', project: 'forge', label: 'lead' },
        path: '/proj/docs/plan.md',
        reviewable: false,
        connection: untouched(),
      },
    }).body;
    expect(html).toContain('Opening /proj/docs/plan.md');
    expect(await idsOf(html)).toEqual([]);
  });

  /**
   * The extensions page: the rail's tabs, a plugin row whose fold button is
   * an overlay with its own label, the band's controls and the marketplace
   * rows. Rendered inside a landmark, which is what the page's own `<main>`
   * gives it in the app.
   */
  it('draws the extensions page with no violations', async () => {
    const html = render(ExtensionsBody, {
      props: {
        wire: extensionsWire(),
        refusal: null,
        projects: [project],
        mark: null,
        ...recorder().actions,
      },
    }).body;
    expect(html, 'the plugin rows drew, so axe saw them').toContain('superpowers');
    expect(html, 'and the marketplaces section').toContain('From the marketplaces');
    expect(await idsOf(`<main>${html}</main>`)).toEqual([]);
  });

  /**
   * The page with an unknown inventory: no plugins root resolved, so the
   * read is `null` and the page says so rather than drawing an empty tree.
   */
  it('draws the extensions page with no inventory, with no violations', async () => {
    const html = render(ExtensionsBody, {
      props: {
        wire: extensionsWire({ snapshot: null }),
        refusal: null,
        projects: [project],
        mark: null,
        ...recorder().actions,
      },
    }).body;
    expect(html).toContain('could not read the plugin inventory');
    expect(await idsOf(`<main>${html}</main>`)).toEqual([]);
  });

  /**
   * The settings page: the sub-nav's buttons, the pane's instance cards, and
   * each line's class badge. Rendered inside a landmark, which is what the
   * page's own `<main>` gives it in the app.
   */
  it('draws the settings page with no violations', async () => {
    const html = render(SettingsBody, {
      props: {
        document: settingsWire,
        mark: null,
        cliVersion: '2.1.280',
        selected: null,
        onSelect: () => {},
        draft: { values: {}, adds: {}, removes: {} },
        onToggleEditing: () => {},
        onValue: () => {},
        onEntryValue: () => {},
        onAdd: () => {},
        onRemoveRequest: () => {},
        onCancelAdd: () => {},
        onUndoRemove: () => {},
      },
    }).body;
    expect(html, 'the sub-nav drew').toContain('accounts');
    expect(html, 'and the selected area rows').toContain('accounts.Subspace.token');
    expect(await idsOf(`<main>${html}</main>`)).toEqual([]);
  });

  /** The editor's controls: a text box, a toggle, a select and a list, each
   *  with its own label. */
  it('draws the settings editor with no violations', async () => {
    const html = render(SettingsBody, {
      props: {
        document: settingsWire,
        mark: null,
        cliVersion: '2.1.280',
        selected: 'server',
        onSelect: () => {},
        editing: true,
        draft: { values: { 'server.port': 8791 }, adds: {}, removes: {} },
        onToggleEditing: () => {},
        onValue: () => {},
        onEntryValue: () => {},
        onAdd: () => {},
        onRemoveRequest: () => {},
        onCancelAdd: () => {},
        onUndoRemove: () => {},
      },
    }).body;
    expect(html, 'the number control drew').toContain('type="number"');
    expect(html, 'and the value that was edited').toContain('8791');
    expect(await idsOf(`<main>${html}</main>`)).toEqual([]);
  });

  /** A refused dispatch, in the core's own words, is the page's own line. */
  it('draws the extensions page with a refusal, with no violations', async () => {
    const html = render(ExtensionsBody, {
      props: {
        wire: extensionsWire(),
        refusal: 'a plugin action is already running; wait for it to finish',
        projects: [project],
        mark: null,
        ...recorder().actions,
      },
    }).body;
    expect(html).toContain('a plugin action is already running');
    expect(await idsOf(`<main>${html}</main>`)).toEqual([]);
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
            is_backgrounded: true,
            task_type: 'local_bash',
            description: 'run the gate',
            command: 'cargo test',
            tool_use_id: 'tu-bash',
          },
        ]
      : null,
    false,
    { calls: new Map(), owners: new Map(), stoppedTasks: new Set() },
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
  // The files slip is mandatory (Ved, 2026-10-17), so a seeded strip draws it.
  files.sync(on ? '/Users/me/proj/.claude/worktrees/probe' : null, on ? 'probe' : null);
}
