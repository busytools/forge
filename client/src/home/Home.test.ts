import { render } from 'svelte/server';
import { afterEach, describe, expect, it } from 'vitest';

import { brandPath } from '../brand';
import { homeWire } from '../dev/fixture.data';
import { CLIENT_VERSION, PROTOCOL_VERSION } from '../protocol';
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
  // The fleet is one row per PROJECT, so the gate it draws is the
  // project's own read - the same one the mockup's branch cell sits in.
  return {
    ...homeWire,
    projects: homeWire.projects.map((row) => ({
      ...row,
      work: { branch: null, changed: null, gate },
    })),
  };
};

describe('the home page as it draws', () => {
  /**
   * The core's last fatal draws above everything (#1638): the words are the
   * terminal's own, carried on the home wire.
   */
  it("draws the core's fatal above the header, and nothing when there is none", () => {
    const stopped = draw({
      wire: {
        ...homeWire,
        fatal_error: 'Failed to establish or maintain the Agent SDK bridge connection.',
      },
    });
    expect(stopped, 'the fatal is drawn').toContain(
      'forge stopped: Failed to establish or maintain the Agent SDK bridge connection.',
    );
    expect(
      stopped.indexOf('forge stopped:'),
      'above the header, where a stopped install is the first thing said',
    ).toBeLessThan(stopped.indexOf('class="top"'));
    expect(draw(), 'no fatal, no line').not.toContain('forge stopped:');
  });

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
    const body = draw({ wire: { ...homeWire, projects: [], agents: [], fleet: [] } });
    expect(body, 'an empty fleet drew no empty state').toContain('No projects yet');
    expect(body).toContain('forge.toml');
    expect(body).not.toContain('fleet-row');
  });

  it('draws every project the snapshot holds as its own fleet row', () => {
    const body = draw();
    expect(body).toContain('class="fleet-row"');
    expect(body).toContain('>proj</a>');
    expect(body, 'and the board keeps its own control').toContain('href="/board/TestOrg/proj"');
  });

  /**
   * **The row is the way into the lead's chat** - the way-in the per-seat
   * rows gave before the fleet replaced them: the name's link stretches
   * over the whole row (the sheet draws that), and a seat chip is its own
   * link to its own seat.
   */
  it('leads the row into the lead, and a chip into its seat', () => {
    // Two seats, because with one the chip's target and the row's are the
    // same string - a chip wired to the row's href would pass everything.
    const [first] = homeWire.agents;
    if (first === undefined) throw new Error('the fixture holds no agent');
    const wire: HomeWire = {
      ...homeWire,
      agents: [
        ...homeWire.agents,
        { ...first, slot: { org: 'TestOrg', project: 'proj', label: 'w1' } },
      ],
    };
    const body = draw({ wire });
    expect(body, 'the row opens the lead').toMatch(
      /class="fleet-name fleet-go" href="\/session\/TestOrg\/proj\/lead"/,
    );
    expect(body, 'the lead chip opens the lead').toMatch(
      /class="fleet-seat" href="\/session\/TestOrg\/proj\/lead"/,
    );
    expect(body, 'a worker chip opens its own seat').toMatch(
      /class="fleet-seat" href="\/session\/TestOrg\/proj\/w1"/,
    );
  });

  /**
   * **The age is the row's own cell.** The reshape dropped the per-row age
   * the home used to draw, and it is the cell a reader uses to see a project
   * gone quiet without opening its board - so the fleet row draws when the
   * project last moved, from the newest write among its seats.
   */
  it('draws when the project last moved', () => {
    const idle = { ...homeWire, agents: homeWire.agents.map((row) => ({ ...row, pending: null })) };
    // An epoch-second stamp: however far back the clock's year is read from,
    // the words are days rather than minutes or hours.
    const body = draw({
      wire: {
        ...idle,
        agents: idle.agents.map((row) => ({
          ...row,
          last_activity: { secs_since_epoch: 1_000, nanos_since_epoch: 0 },
        })),
      },
    });
    expect(body, 'the fleet row drew no age').toContain('class="fleet-when"');
    expect(body, "the age is not the row's own words").toMatch(/fleet-when[^>]*>\d+d</);

    // A live seat with nothing written yet reads `now`; `never` is for a
    // project with neither a seat nor a session behind it (the view test
    // holds that half, where the catalog can be built by hand).
    expect(draw({ wire: idle }), 'a seat with no write drew no age').toMatch(
      /fleet-when[^>]*>now</,
    );
  });

  /**
   * The way into the models page, which the view test cannot see: it asserts
   * the field, not that a component draws what the field says. The card is
   * the whole target, and the chevron is drawn at rest - an affordance only
   * the pointer uncovers is what the standard forbids.
   */
  it('opens the models page from the dictation card', () => {
    const body = draw();

    expect(body).toContain('href="/models"');
    expect(body, 'the door draws no marker a reader can see').toContain('i-chev');
    // One door, not four: the band's other cards are facts about this forge.
    expect(body.match(/href="\/models"/g)).toHaveLength(1);
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
  it('encodes the project its board links to', () => {
    const wire = {
      ...homeWire,
      projects: homeWire.projects.map((row) => ({
        ...row,
        project: { ...row.project, name: 'two words' },
      })),
      fleet: homeWire.fleet.map((row) => ({ ...row, project: 'two words' })),
    };
    expect(draw({ wire })).toContain('href="/board/TestOrg/two%20words"');
  });

  it('draws the fleet mark the strongest seat state names', () => {
    const body = draw();
    // The fixture's lead holds a permission prompt beside an Idle lifecycle -
    // a pairing this fixture was hand-made with, since the production shape of
    // an ask is a running seat - so the ask is the strongest seat state and
    // the fleet row wears it (#1885), in the same vocabulary a seat row draws.
    expect(body, 'the fleet row drew no strongest-state class').toContain('class="row needs"');
    expect(body, 'the fleet row drew no ask mark').toContain('class="dot warn"');
    expect(body, 'the seats ride the row').toContain('fleet-seat');
    expect(body, 'and the unnamed miss is named').toContain('w1 holds no row');
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

  /**
   * A browser tab installs nothing, so this line is the two facts a reader
   * asked for - which build is running, and what is published - and it is
   * text where the shell's same-shaped line is a control.
   */
  it('names the web build and the release published beside it', () => {
    updateState.set({ stage: 'web', latest: '9.9.9' });

    const body = draw();
    expect(body).toContain(`client v${CLIENT_VERSION}`);
    expect(body).toContain('latest v9.9.9');
    expect(body, 'a browser build drew a control it cannot use').not.toMatch(
      /<button[^>]*>client /,
    );
  });

  it('names the web build alone when nothing newer is published', () => {
    updateState.set({ stage: 'web', latest: null });

    const body = draw();
    expect(body).toContain(`client v${CLIENT_VERSION}`);
    expect(body).not.toContain('latest v');
  });
});
