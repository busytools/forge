import { render } from 'svelte/server';
import { afterEach, describe, expect, it } from 'vitest';

import { brandPath } from '../brand';
import { homeWire } from '../dev/fixture.data';
import { PROTOCOL_VERSION } from '../protocol';
import { updateState } from '../update/state';
import type { Gate, HomeWire } from '../wire/home';
import Home from './Home.svelte';

/** `wire` is required of the component, so the fixture is the test's default. */
const draw = (props: Record<string, unknown> = {}) =>
  render(Home, { props: { wire: homeWire, ...props } }).body;

const withCli = (installed: string | null, latest: string | null): HomeWire => ({
  ...homeWire,
  cli_version: { installed, latest },
});

/**
 * The fixture's one agent, with its own work read answering the given gate.
 *
 * The pending column is cleared to reach it: the fixture's row waits on a
 * permission, which is first in the cell's chain, so every cell below it -
 * the gate line and the refusal both - is unreachable while it is set. That
 * is a property of the fixture rather than of the page, and the fixture is
 * the server's committed blob byte for byte, so it cannot be widened here.
 */
const withGate = (gate: Gate): HomeWire => {
  // The gate a row draws is the SEAT's own read, so it is set on the agent and
  // not on the project beside it: a page reading the project's would draw the
  // fixture's `gone` for every gate below.
  return {
    ...homeWire,
    agents: homeWire.agents.map((row) => ({
      ...row,
      pending: null,
      work: { branch: null, changed: null, gate },
    })),
  };
};

describe('the home page as it draws', () => {
  /**
   * The mark on the page is the one the server sent. A prop the component
   * does not declare is dropped without a word, so the page would fall back
   * to the built-in and still look right.
   */
  it('draws the mark the server sent, not the built-in', () => {
    const body = draw({ mark: 'klin' });
    expect(body, 'the server sent klin and the page drew the built-in').toContain(
      brandPath('klin'),
    );
    expect(body).not.toContain(brandPath(null));
  });

  it('draws the built-in when the server sent no mark', () => {
    expect(draw({ mark: null })).toContain(brandPath(null));
  });

  /**
   * The notice is a thing to act on, so it is drawn only when npm has a
   * newer version than the installed CLI. Drawn whenever `latest` resolved,
   * an ordinary machine whose CLI is current reads `claude 2.1.280 · up
   * v2.1.280 available`.
   */
  /**
   * The socket protocol reads with the two builds it binds: the client and a
   * server that disagrees on it refuse each other (`socket.ts` checks the
   * greeting), so the number belongs where a mismatch would be looked for.
   */
  it('names the socket protocol in the header', () => {
    expect(draw(), 'the protocol this app speaks').toContain(`socket v${PROTOCOL_VERSION}`);
  });

  it('announces an update only when the published version is newer', () => {
    expect(draw({ wire: withCli('2.1.280', '2.1.290') })).toContain('available');
    expect(
      draw({ wire: withCli('2.1.280', '2.1.280') }),
      'the CLI is current and the page still claims an update',
    ).not.toContain('available');
    expect(
      draw({ wire: withCli(null, '2.1.290') }),
      'the installed probe failed, so available is unanswerable',
    ).not.toContain('available');
    expect(draw({ wire: withCli('2.1.290', '2.1.280') })).not.toContain('available');
  });

  /**
   * An empty fleet is a state, not a blank page: the shell and the copy are
   * the difference between "nothing configured" and "the page is broken".
   */
  /**
   * A row whose tree could not be read says so, which is a claim the book page
   * made while nothing rendered it: `gateLine` was written, documented and
   * tested as a string, and a `gone` row drew byte-identically to an ordinary
   * one. Asserting the string is what let that ship, so this asserts the page.
   */
  it('says on the row why there is no branch to show', () => {
    expect(draw({ wire: withGate('gone') })).toContain('its working directory is not there');
    expect(draw({ wire: withGate('not_a_repository') })).toContain('not a git repository');
    // And a readable tree draws none of it, so the line means what it says.
    expect(draw({ wire: withGate('in_repo') })).not.toContain('its working directory is not there');
  });

  it('draws its shell and says so when the server holds no projects', () => {
    const body = draw({ wire: { ...homeWire, projects: [], agents: [] } });
    expect(body, 'an empty fleet drew no empty state').toContain('No projects yet');
    expect(body).toContain('forge.toml');
    expect(body).not.toContain('class="list"');
  });

  it('draws every project the snapshot holds', () => {
    expect(draw()).toContain('class="list"');
  });

  /**
   * The row's mark reaches the page, which the view test cannot see: it
   * asserts the mapping, not that a component draws what the mapping says.
   */
  /**
   * The link a row carries is built by `hrefForSlot`, and the fixture's
   * label is `lead`, so a raw template in place of the encoder passes every
   * other test. A label with a space in it is what tells them apart.
   */
  it('encodes the seat a row links to', () => {
    const wire = {
      ...homeWire,
      agents: [
        {
          ...homeWire.agents[0],
          // The fixture's project, so the row is drawn at all: an agent whose
          // slot names no project in `projects` contributes no row.
          slot: { org: 'TestOrg', project: 'proj', label: 'two words' },
        },
      ],
    };
    expect(draw({ wire })).toContain('href="/session/TestOrg/proj/two%20words"');
  });

  it('draws the mark its state names on the row', () => {
    const body = draw();
    expect(body, 'the row drew no lifecycle class').toContain('class="row idle"');
    expect(body).toContain('class="dot live"');
    expect(body, 'the row drew no link to its seat').toContain('href="/session/TestOrg/proj/lead"');
  });
});

describe("the client's own update", () => {
  afterEach(() => {
    updateState.set({ stage: 'current' });
  });

  it('draws the version an update would install, as a control', () => {
    updateState.set({ stage: 'available', version: '9.9.9' });

    const body = draw();
    expect(body).toContain('v9.9.9 available');
    expect(body, 'the notice is text where it should be a control').toMatch(/<button[^>]*>client /);
  });

  it('draws none of it on a build that is current', () => {
    expect(draw()).not.toContain('v9.9.9');
  });

  it('draws the installing stage, offering nothing to press', () => {
    updateState.set({ stage: 'installing', version: '9.9.9' });

    const body = draw();
    expect(body).toContain('v9.9.9 updating...');
    expect(body, 'the stage offers an action while one is already in flight').not.toMatch(
      /<button[^>]*>client /,
    );
  });

  it('offers the restart once the update is installed', () => {
    updateState.set({ stage: 'restart', version: '9.9.9' });

    expect(draw()).toContain('restart to finish');
  });

  it('offers the installer once the download is checked on the phone', () => {
    updateState.set({ stage: 'install', version: '9.9.9' });

    expect(draw()).toContain('ready - install it');
  });

  it('draws a failed install with a retry, and its reason on the control', () => {
    updateState.set({ stage: 'failed', version: '9.9.9', detail: 'the signature did not match' });

    const body = draw();
    expect(body).toContain('update failed, retry');
    expect(body, 'the reason the install failed was dropped').toContain(
      'title="the signature did not match"',
    );
  });
});
