/** A settings document for the page's tests: every class, an empty area and a
 *  key-major account table, which is the shape the server builds. */

import type { SettingsDocument, SettingsKey } from '../wire/settings';

function key(
  path: string,
  held: SettingsKey['class'],
  value: SettingsKey['value'],
  label?: string,
): SettingsKey {
  return { path, class: held, classLabel: label ?? held, value };
}

export const settingsWire: SettingsDocument = {
  version: 'v1.1.34+abc1234',
  source: '~/.claude/forge/forge.toml',
  totals: { keys: 7, areas: 4 },
  areas: [
    {
      id: 'accounts',
      name: 'accounts',
      note: 'the provider endpoints and credentials a session can spawn under',
      keys: [
        key('accounts.Subspace.display_name', 'live', { kind: 'string', text: 'Subspace' }),
        key('accounts.Granite.display_name', 'live', { kind: 'string', text: 'Granite' }),
        key('accounts.Subspace.token', 'live', { kind: 'string', text: 'a-set-token' }),
        key('accounts.Granite.token', 'live', { kind: 'string', text: 'another-set-token' }),
        key('accounts.Granite.env.FLAG', 'next_session', { kind: 'string', text: '1' }),
        key('accounts.Granite.models', 'live', { kind: 'list', items: ['a', 'b'] }),
      ],
    },
    {
      id: 'server',
      name: 'server',
      note: 'the socket a client reads through',
      keys: [
        key('server.port', 'restart', { kind: 'number', text: '8790' }),
        key('server.first_frame_cap_kb', 'live', { kind: 'number', text: '512' }),
      ],
    },
    {
      id: 'connectors',
      name: 'connectors',
      note: 'the integrations that reach outside forge',
      keys: [
        key('plugins.auto_update', 'not_read_yet', { kind: 'bool', text: 'true' }),
        key('gotify.later', 'other', { kind: 'string', text: 'x' }, 'some future class'),
      ],
    },
    { id: 'models', name: 'models', note: 'per-model parameters', keys: [] },
  ],
};

/** A home read with nothing in it, for the page's header's claude version. */
export const emptyHome = { wire: null, refused: null, failed: null, report: null };
