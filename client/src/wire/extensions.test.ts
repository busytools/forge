import { describe, expect, it } from 'vitest';

import { extensionsFrom, updateRunFrom } from './extensions';

/**
 * The read's own narrowing, where the unknown values enter.
 *
 * A run status or a trigger this client is older than must land as `other`
 * rather than as `queued` or `manual`: the page switches over them
 * exhaustively, and a fallback that guesses would draw a row as something
 * the server never said.
 */
describe('extensionsFrom', () => {
  it('narrows a run status this client does not know to other', () => {
    const read = extensionsFrom({
      snapshot: null,
      update_run: {
        trigger: 'manual',
        finished: false,
        rows: [{ plugin_id: 'a@probe', scope: 'user', status: 'a_new_state' }],
      },
      lsp: [],
    });
    expect(read.update_run?.rows[0]?.status).toBe('other');
  });

  it('narrows a trigger this client does not know to other', () => {
    const run = updateRunFrom({ trigger: 'scheduled', finished: true, rows: [] });
    expect(run?.trigger).toBe('other');
  });

  it('keeps a snapshot that is absent as null rather than as an empty one', () => {
    const read = extensionsFrom({ snapshot: null, update_run: null, lsp: [] });
    expect(read.snapshot).toBeNull();
    expect(read.update_run).toBeNull();
  });

  it('reads the components it knows and defaults the fields it does not', () => {
    const read = extensionsFrom({
      snapshot: {
        components: [
          {
            plugin: 'demo@probe',
            marketplace: 'probe',
            installed: true,
            skills: ['one', 7, 'two'],
            lsp_servers: { 'rust-analyzer': 'rust-analyzer', broken: 4 },
          },
        ],
        marketplaces: [{ name: 'probe', source: 'github' }],
      },
      lsp: [{ plugin: 'demo@probe', server: 'rust-analyzer' }],
    });
    const entry = read.snapshot?.components[0];
    expect(entry?.skills).toEqual(['one', 'two']);
    expect(entry?.lsp_servers['rust-analyzer']).toBe('rust-analyzer');
    expect(entry?.lsp_servers['broken']).toBeUndefined();
    expect(entry?.enabled).toBe(true);
    expect(entry?.auto).toBe(false);
    expect(entry?.load_error).toBeNull();
    expect(read.lsp[0]?.on_path).toBe(false);
  });
});
