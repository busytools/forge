import { describe, expect, it } from 'vitest';

import { settingsWire } from '../dev/settings.fixture';
import type { SettingsArea } from '../wire/settings';
import { builtInFor, classesOf, groupsOf, selectedArea } from './view';

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

  it('groups a key-major account table by the instance the server names', () => {
    const groups = groupsOf(area('accounts'));
    expect(
      groups.map((group) => group.label),
      'a dotted instance stays one group',
    ).toEqual(['Acme.Corp', 'Granite', 'OpenRouter', 'Subspace']);
    const granite = groups.find((group) => group.label === 'Granite');
    expect(granite?.rows.map((row) => row.path)).toEqual([
      'accounts.Granite.display_name',
      'accounts.Granite.token',
      'accounts.Granite.env.FLAG',
      'accounts.Granite.models',
    ]);
  });

  it('names the built-in a client axis falls back to', () => {
    expect(builtInFor('client', 'client.mark'), 'an unset mark draws the shipped mark').toBe(
      'built-in panes',
    );
    expect(builtInFor('client', 'client.theme'), 'an unset theme draws the shipped palette').toBe(
      'built-in dark',
    );
    expect(
      builtInFor('client', 'client.font'),
      'the bundled face has no settable name, so the cell does not print one',
    ).toBe('the bundled face');
    expect(builtInFor('server', 'server.port'), 'no other area has a client default').toBeNull();
  });

  it('groups models by the model name, dots and all', () => {
    expect(
      groupsOf(area('models')).map((group) => group.label),
      'the model is the visible identifier, not the tooltip',
    ).toEqual(['claude-opus-5-5', 'deepseek-v4.1-flash']);
  });

  it('splits the server area into its two tables', () => {
    expect(groupsOf(area('server')).map((group) => group.label)).toEqual(['browser', 'server']);
  });

  it('draws the voice area with its dictate and tts groups', () => {
    // The server groups the two tables into one area; the page passes the
    // area through and draws the groups the server names.
    const voice: SettingsArea = {
      id: 'voice',
      name: 'voice',
      note: 'the two directions: a take becomes text, and a reply becomes speech',
      keys: [
        {
          path: 'dictate.enabled',
          leaf: 'enabled',
          group: ['dictate'],
          pattern: 'dictate.enabled',
          class: 'restart',
          classLabel: 'restart',
          value: { kind: 'bool', text: 'true' },
          edit: { kind: 'bool', allowed: [], required: false, group: null },
        },
        {
          path: 'tts.enabled',
          leaf: 'enabled',
          group: ['tts'],
          pattern: 'tts.enabled',
          class: 'restart',
          classLabel: 'restart',
          value: { kind: 'bool', text: 'false' },
          edit: { kind: 'bool', allowed: [], required: false, group: null },
        },
      ],
    };
    expect(groupsOf(voice).map((group) => group.label)).toEqual(['dictate', 'tts']);
  });

  it('lists every class an area holds, in the legend order', () => {
    expect(
      classesOf(area('connectors').keys),
      'the unknown class and the unread key are both drawn, not only the furthest',
    ).toEqual(['other', 'not_read_yet']);
  });

  it('draws the leaf and group the server sends, dots and all', () => {
    const slug = area('accounts').keys.find(
      (key) => key.path === 'accounts.OpenRouter.model_slugs.glm-5.3-flash',
    );
    expect(slug?.leaf, 'a dotted leaf is one name, not the tail of its last dot').toBe(
      'glm-5.3-flash',
    );
    expect(slug?.group).toEqual(['OpenRouter']);
    const dotted = area('accounts').keys.find((key) => key.path === 'accounts.Acme.Corp.token');
    expect(dotted?.group, 'a dotted instance stays one group').toEqual(['Acme.Corp']);
  });
});
