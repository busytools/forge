/**
 * A settings document for the page's tests: every class, dotted names in
 * both the group and the leaf, two grouped tables, an empty area, a
 * key-major account table, and the edit metadata the editor draws.
 *
 * A fixture, so it lives beside the dev route's own data rather than under
 * the page it feeds: nothing the shipped app imports may reach this file.
 */

import type { SettingsDocument, SettingsEditSpec, SettingsKey } from '../wire/settings';

/** A plain text control, for a fixture key the editor tests do not exercise. */
const TEXT: SettingsEditSpec = { kind: 'string', allowed: [], required: false, group: null };
const NUMBER: SettingsEditSpec = { kind: 'number', allowed: [], required: true, group: null };
const BOOLEAN: SettingsEditSpec = { kind: 'bool', allowed: [], required: false, group: null };
const LIST: SettingsEditSpec = { kind: 'list', allowed: [], required: false, group: null };

function key(
  path: string,
  held: SettingsKey['class'],
  value: SettingsKey['value'],
  group: string[],
  leaf: string = path.slice(path.lastIndexOf('.') + 1),
  label: string = held,
  edit: SettingsEditSpec = TEXT,
): SettingsKey {
  return { path, leaf, group, pattern: path, class: held, classLabel: label, value, edit };
}

export const settingsWire: SettingsDocument = {
  version: '1.1.34+abc1234',
  source: '~/.claude/forge/forge.toml',
  fingerprint: '0123456789abcdef',
  totals: { keys: 15, areas: 5 },
  areas: [
    {
      id: 'accounts',
      name: 'accounts',
      note: 'the provider endpoints and credentials a session can spawn under',
      keys: [
        key('accounts.Subspace.display_name', 'live', { kind: 'string', text: 'Subspace' }, [
          'Subspace',
        ]),
        key('accounts.Granite.display_name', 'live', { kind: 'string', text: 'Granite' }, [
          'Granite',
        ]),
        key('accounts.Subspace.token', 'live', { kind: 'string', text: 'a-set-token' }, [
          'Subspace',
        ]),
        key('accounts.Granite.token', 'live', { kind: 'string', text: 'another-set-token' }, [
          'Granite',
        ]),
        key(
          'accounts.Granite.env.FLAG',
          'next_session',
          { kind: 'string', text: '1' },
          ['Granite'],
          'FLAG',
        ),
        key(
          'accounts.Granite.models',
          'live',
          { kind: 'list', items: ['a', 'b'] },
          ['Granite'],
          'models',
          'live',
          LIST,
        ),
        key(
          'accounts.OpenRouter.model_slugs.glm-5.3-flash',
          'live',
          { kind: 'string', text: 'z-ai/glm-5.3-flash' },
          ['OpenRouter'],
          'glm-5.3-flash',
        ),
        key('accounts.Acme.Corp.token', 'live', { kind: 'string', text: 'a-dotted-name' }, [
          'Acme.Corp',
        ]),
      ],
    },
    {
      id: 'server',
      name: 'server',
      note: 'the socket a client reads through, and the browser the server hosts',
      keys: [
        key(
          'server.port',
          'restart',
          { kind: 'number', text: '8790' },
          ['server'],
          'port',
          'restart',
          NUMBER,
        ),
        key(
          'server.first_frame_cap_kb',
          'live',
          { kind: 'number', text: '512' },
          ['server'],
          'first_frame_cap_kb',
          'live',
          NUMBER,
        ),
        key(
          'browser.idle_minutes',
          'live',
          { kind: 'number', text: '30' },
          ['browser'],
          'idle_minutes',
          'live',
          NUMBER,
        ),
      ],
    },
    {
      id: 'connectors',
      name: 'connectors',
      note: 'the integrations that reach outside forge',
      keys: [
        key(
          'plugins.auto_update',
          'not_read_yet',
          { kind: 'bool', text: 'true' },
          ['plugins'],
          'auto_update',
          'not_read_yet',
          BOOLEAN,
        ),
        key(
          'gotify.later',
          'other',
          { kind: 'string', text: 'x' },
          ['gotify'],
          'later',
          'some future class',
        ),
      ],
    },
    {
      id: 'models',
      name: 'models',
      note: 'per-model parameters',
      keys: [
        key(
          'models.deepseek-v4.1-flash.auto_compact_window',
          'next_session',
          { kind: 'number', text: '983040' },
          ['deepseek-v4.1-flash'],
          'auto_compact_window',
          'next_session',
          NUMBER,
        ),
        key(
          'models.claude-opus-5-5.auto_compact_window',
          'next_session',
          { kind: 'number', text: '1048576' },
          ['claude-opus-5-5'],
          'auto_compact_window',
          'next_session',
          NUMBER,
        ),
      ],
    },
    { id: 'documents', name: 'documents', note: 'the document hand-off', keys: [] },
  ],
};

/** A home read with nothing in it, for the page's header's claude version. */
export const emptyHome = { wire: null, refused: null, failed: null, report: null };
