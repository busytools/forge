import { describe, expect, it } from 'vitest';

import { settingsWire } from './testing';
import { builtInFor, groupsOf, selectedArea, strongest } from './view';

function area(id: string) {
  const found = settingsWire.areas.find((candidate) => candidate.id === id);
  if (found === undefined) throw new Error(`the fixture has no ${id} area`);
  return found;
}

describe('the settings page view', () => {
  it('draws the first area when nothing is selected', () => {
    const { area, missing } = selectedArea(settingsWire, null);
    expect(area?.id, 'the sub-nav starts on the first area').toBe('accounts');
    expect(missing).toBe(false);
  });

  it('reports a selected area the document no longer holds', () => {
    const { area, missing } = selectedArea(settingsWire, 'dictate');
    expect(area, 'nothing is drawn for an area that is gone').toBeNull();
    expect(missing, 'and the pane says it is gone rather than drawing the first').toBe(true);
  });

  it('draws nothing for a document with no areas', () => {
    const { area, missing } = selectedArea({ ...settingsWire, areas: [] }, 'accounts');
    expect(area).toBeNull();
    expect(missing, 'an empty document is not an unknown area').toBe(false);
  });

  it('groups a key-major account table by account', () => {
    const groups = groupsOf(area('accounts'));
    expect(groups.map((group) => group.label)).toEqual(['Granite', 'Subspace']);
    expect(groups[1]?.rows.map((row) => row.path)).toEqual([
      'accounts.Subspace.display_name',
      'accounts.Subspace.token',
    ]);
    expect(groups[0]?.rows.map((row) => row.path)).toEqual([
      'accounts.Granite.display_name',
      'accounts.Granite.token',
      'accounts.Granite.env.FLAG',
      'accounts.Granite.models',
    ]);
  });

  it('wears the furthest class a group holds', () => {
    const groups = groupsOf(area('accounts'));
    expect(groups[1]?.class, 'a group with only live keys reads live').toBe('live');
    expect(groups[0]?.class, 'a next-session key outranks its live siblings').toBe('next_session');
  });

  it('names the built-in a client axis falls back to', () => {
    expect(builtInFor('client', 'client.mark'), 'an unset mark draws panes').toBe('panes');
    expect(builtInFor('client', 'client.theme'), 'an unset theme draws dark').toBe('dark');
    expect(builtInFor('client', 'client.font'), 'an unset font draws Fira Code').toBe('Fira Code');
    expect(builtInFor('server', 'server.port'), 'no other area has a client default').toBeNull();
  });

  it('orders the classes by what they cost to apply', () => {
    expect(strongest(area('connectors').keys), 'an unread key outranks an unknown class').toBe(
      'not_read_yet',
    );
  });
});
