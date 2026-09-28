import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import { brandPath } from '../brand';
import { homeWire } from '../dev/fixture.data';
import type { HomeWire } from '../wire/home';
import Home from './Home.svelte';

/** `wire` is required of the component, so the fixture is the test's default. */
const draw = (props: Record<string, unknown> = {}) =>
  render(Home, { props: { wire: homeWire, ...props } }).body;

const withCli = (installed: string | null, latest: string | null): HomeWire => ({
  ...homeWire,
  cli_version: { installed, latest },
});

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
