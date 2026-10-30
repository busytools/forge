import { describe, expect, it } from 'vitest';

import { classWords, settingsDocumentFrom } from './settings';

/** A wire frame with one key of each shape the page draws. */
function frame(): unknown {
  return {
    version: 'v1.1.34',
    source: '~/.claude/forge/forge.toml',
    totals: { keys: 2, areas: 1 },
    areas: [
      {
        id: 'server',
        name: 'server',
        note: 'the socket',
        keys: [
          { path: 'server.port', kind: 'number', value: 8790, class: 'restart' },
          { path: 'accounts.Primary.models', kind: 'list', value: ['a', 'b'], class: 'live' },
        ],
      },
    ],
  };
}

describe('the settings frame', () => {
  it('narrows a document into its areas, paths and values', () => {
    const document = settingsDocumentFrom(frame());
    expect(document, 'a well-formed frame narrows').not.toBeNull();
    expect(document?.areas).toHaveLength(1);
    expect(document?.areas[0]?.keys[0]?.path).toBe('server.port');
    expect(document?.areas[0]?.keys[0]?.value).toEqual({ kind: 'number', text: '8790' });
    expect(document?.areas[0]?.keys[1]?.value).toEqual({ kind: 'list', items: ['a', 'b'] });
    expect(document?.totals).toEqual({ keys: 2, areas: 1 });
  });

  it('keeps a class this client does not know as its own word', () => {
    const document = settingsDocumentFrom({
      areas: [{ id: 'a', keys: [{ path: 'a.b', kind: 'string', value: 'x', class: 'future' }] }],
    });
    const key = document?.areas[0]?.keys[0];
    expect(key, 'the key drew').toBeDefined();
    expect(key?.class, 'an unknown class is not silently live').toBe('other');
    expect(
      key === undefined ? null : classWords(key.class, key.classLabel),
      'and the word it arrived with is drawn',
    ).toBe('future');
  });

  it('keeps a kind this client does not know as the JSON it arrived as', () => {
    const document = settingsDocumentFrom({
      areas: [
        { id: 'a', keys: [{ path: 'a.b', kind: 'shape', value: { deep: 1 }, class: 'live' }] },
      ],
    });
    expect(document?.areas[0]?.keys[0]?.value).toEqual({
      kind: 'other',
      text: '{"deep":1}',
    });
  });

  it('draws an unset value as unset rather than as a name', () => {
    const document = settingsDocumentFrom({
      areas: [
        {
          id: 'client',
          keys: [{ path: 'client.font', kind: 'string', value: null, class: 'live' }],
        },
      ],
    });
    expect(document?.areas[0]?.keys[0]?.value, 'null is the file not setting the key').toEqual({
      kind: 'unset',
    });
  });

  it('draws a table value as its pairs', () => {
    const document = settingsDocumentFrom({
      areas: [
        {
          id: 'a',
          keys: [{ path: 'a.b', kind: 'table', value: { k: 'v' }, class: 'live' }],
        },
      ],
    });
    expect(document?.areas[0]?.keys[0]?.value).toEqual({
      kind: 'table',
      rows: [{ key: 'k', value: 'v' }],
    });
  });

  it('answers null for a frame that is not an object', () => {
    expect(settingsDocumentFrom('not a document')).toBeNull();
    expect(settingsDocumentFrom([1, 2])).toBeNull();
  });

  it('counts the areas itself when the frame carries no totals', () => {
    const document = settingsDocumentFrom({
      areas: [
        { id: 'a', keys: [{ path: 'a.b', kind: 'string', value: 'x', class: 'live' }] },
        { id: 'b', keys: [] },
      ],
    });
    expect(document?.totals, 'the header never draws a zero it did not count').toEqual({
      keys: 1,
      areas: 2,
    });
  });
});
