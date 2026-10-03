import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import { homeView, type Row as RowModel } from '../home/view';
import Row from './Row.svelte';

/**
 * A row's name is the way into the seat it names - except where there is no
 * seat behind it to open. A sleeping seat's page draws a refusal, and the
 * terminal draws those rows as labels with no hit target, so neither may
 * this one.
 *
 * Read as markup, because whether a link is drawn at all is a question about
 * the tree. The seat's own path is what is asserted, so a row's artifact
 * anchor (an external URL) cannot stand in for it.
 */

/** The fixture's lead row, in the state named. */
function rowIn(state: RowModel['state']): RowModel {
  const first = homeView(homeWire, '127.0.0.1:8790').orgs[0]?.projects[0];
  if (first === undefined) throw new Error('the fixture holds no project');
  return { ...first.lead, state };
}

const body = (row: RowModel): string => render(Row, { props: { row, now: 0 } }).body;

describe('a row that names a seat', () => {
  it('links a seat that has something behind it', () => {
    expect(body(rowIn({ kind: 'lifecycle', lifecycle: 'Running' }))).toContain('href="/session/');
  });

  it('draws a sleeping seat as a label rather than a link', () => {
    const row = rowIn({ kind: 'lifecycle', lifecycle: 'Sleeping' });
    const held = body(row);
    expect(held, 'a sleeping row was drawn as a way in').not.toContain('href="/session/');
    expect(held, 'the row stopped naming the seat').toContain(row.name);
  });

  /** The other state the core reports for a seat whose session is gone. */
  it('draws a signed-out seat the same way', () => {
    expect(body(rowIn({ kind: 'lifecycle', lifecycle: 'LoggedOut' }))).not.toContain(
      'href="/session/',
    );
  });

  /**
   * A project nothing has ever run in IS a way in: its page starts the lead,
   * which is the one seat the core can start by name.
   */
  it('links a project nothing has started', () => {
    expect(body(rowIn({ kind: 'never-started' }))).toContain('href="/session/');
  });
});
