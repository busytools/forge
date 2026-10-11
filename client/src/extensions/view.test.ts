import { describe, expect, it } from 'vitest';

import type {
  ExtensionsRead,
  LspCheck,
  MarketplaceHealth,
  MarketplaceSource,
  PluginComponents,
  UpdateRun,
} from '../wire/extensions';
import {
  availableRows,
  band,
  foldGroups,
  kindRows,
  marketplaceDetail,
  marketplaceRows,
  pluginMark,
  pluginName,
  pluginRows,
  railCounts,
  stalePlugins,
} from './view';

function components(over: Partial<PluginComponents> = {}): PluginComponents {
  return {
    plugin: 'demo@probe',
    marketplace: 'probe',
    version: '1.0.0',
    installed: true,
    enabled: true,
    auto: false,
    available_version: '1.0.0',
    skills: [],
    agents: [],
    commands: [],
    hooks: [],
    mcp: false,
    lsp_servers: {},
    load_error: null,
    ...over,
  };
}

function read(
  components_: PluginComponents[],
  over: {
    update_run?: UpdateRun | null;
    lsp?: LspCheck[];
    marketplaces?: MarketplaceSource[];
    health?: MarketplaceHealth[];
  } = {},
): ExtensionsRead {
  return {
    snapshot: {
      installed: [],
      marketplace: [],
      marketplaces: over.marketplaces ?? [],
      components: components_,
      marketplace_health: over.health ?? [],
    },
    update_run: over.update_run ?? null,
    lsp: over.lsp ?? [],
  };
}

function run(over: Partial<UpdateRun> = {}): UpdateRun {
  return { trigger: 'manual', finished: false, rows: [], ...over };
}

describe('the rail counts', () => {
  it('counts only installed rows, so an available plugin does not inflate a number', () => {
    const counts = railCounts(
      read([
        components({ skills: ['a', 'b'], agents: ['x'], hooks: ['SessionStart'] }),
        components({
          plugin: 'available@probe',
          installed: false,
          enabled: false,
          skills: ['c', 'd', 'e'],
        }),
      ]),
    );
    expect(counts.installed).toBe(1);
    expect(counts.skills).toBe(2);
    expect(counts.agents).toBe(1);
    expect(counts.hooks).toBe(1);
  });

  it('counts marketplaces from the configured list', () => {
    const counts = railCounts(
      read([], {
        marketplaces: [
          { name: 'one', source: 'github', repo: null, install_location: null },
          { name: 'two', source: 'github', repo: null, install_location: null },
        ],
      }),
    );
    expect(counts.marketplaces).toBe(2);
  });
});

describe('the plugin rows', () => {
  it('marks a row stale only when the marketplace reports a different version', () => {
    const rows = pluginRows(
      read([
        components({ plugin: 'stale@probe', available_version: '1.1.0' }),
        components({ plugin: 'current@probe' }),
        components({ plugin: 'nocatalog@probe', available_version: null }),
        components({ plugin: 'equal@probe', version: null, available_version: null }),
      ]),
    );
    expect(rows[0]?.availableVersion).toBe('1.1.0');
    expect(rows[1]?.availableVersion).toBeNull();
    expect(rows[2]?.availableVersion).toBeNull();
    expect(rows[3]?.availableVersion).toBeNull();
  });

  it('leaves a disabled plugin out of the stale set even when its versions differ', () => {
    const stale = stalePlugins(read([components({ enabled: false, available_version: '2.0.0' })]));
    expect(stale).toEqual([]);
  });

  it('carries a load failure with its reason, and never drops the row', () => {
    const rows = pluginRows(read([components({ load_error: 'install directory is gone' })]));
    expect(rows).toHaveLength(1);
    expect(rows[0]?.loadError).toBe('install directory is gone');
  });

  it('names a plugin by the part of its id before the marketplace', () => {
    expect(pluginName('superpowers@claude-plugins-official')).toBe('superpowers');
    expect(pluginName('standalone')).toBe('standalone');
  });

  it('derives the mark from the run first, then the row', () => {
    const rows = pluginRows(
      read([components({ available_version: '1.1.0' })], {
        update_run: run({
          rows: [
            {
              plugin_id: 'demo@probe',
              scope: 'user',
              status: 'updating',
              installed_version: '1.0.0',
              available_version: '1.1.0',
              detail: null,
            },
          ],
        }),
      }),
    );
    expect(rows[0] === undefined ? null : pluginMark(rows[0])).toBe('running');
  });

  it("folds a plugin's components into groups, with a hook row naming its triggers", () => {
    const groups = foldGroups(
      components({
        skills: ['one', 'two'],
        hooks: ['SessionStart', 'PreToolUse'],
        lsp_servers: { gopls: 'gopls' },
      }),
    );
    expect(groups.map((group) => group.kind)).toEqual(['skills', 'hooks', 'lsp']);
    expect(groups[0]?.count).toBe(2);
    expect(groups[1]?.rows[0]?.note).toBe('SessionStart, PreToolUse');
  });
});

describe('the updates band', () => {
  it('says nothing when nothing is stale', () => {
    expect(band(read([components()]))).toBeNull();
  });

  it('names the stale plugins with their version pairs', () => {
    const shown = band(read([components({ plugin: 'stale@probe', available_version: '1.1.0' })]));
    expect(shown?.kind).toBe('stale');
    expect(shown?.line).toBe('1 plugin has an update');
    expect(shown?.detail).toBe('stale 1.0.0 → 1.1.0');
  });

  it('reports a run in flight over the stale set it is applying', () => {
    const shown = band(
      read([components({ available_version: '1.1.0' })], {
        update_run: run({
          rows: [
            {
              plugin_id: 'demo@probe',
              scope: 'user',
              status: 'updated',
              installed_version: '1.1.0',
              available_version: '1.1.0',
              detail: null,
            },
            {
              plugin_id: 'other@probe',
              scope: 'user',
              status: 'updating',
              installed_version: '1.0.0',
              available_version: '1.1.0',
              detail: null,
            },
          ],
        }),
      }),
    );
    expect(shown?.kind).toBe('running');
    expect(shown?.detail).toBe('1 of 2 done');
  });

  it('reports a finished run by what it did, failures named', () => {
    const shown = band(
      read([], {
        update_run: run({
          finished: true,
          rows: [
            {
              plugin_id: 'a@probe',
              scope: 'user',
              status: 'updated',
              installed_version: '1.1.0',
              available_version: '1.1.0',
              detail: null,
            },
            {
              plugin_id: 'b@probe',
              scope: 'user',
              status: 'failed',
              installed_version: '1.0.0',
              available_version: '1.1.0',
              detail: 'not found',
            },
          ],
        }),
      }),
    );
    expect(shown?.kind).toBe('report');
    expect(shown?.line).toBe('1 updated, 1 failed');
  });

  it('reads a finished check as how many are behind, not as an update run', () => {
    const shown = band(
      read([], {
        update_run: run({
          finished: true,
          rows: [
            {
              plugin_id: 'a@probe',
              scope: 'user',
              status: 'update_available',
              installed_version: '1.0.0',
              available_version: '1.1.0',
              detail: null,
            },
          ],
        }),
      }),
    );
    expect(shown?.line).toBe('1 plugin behind the marketplace');
  });
});

describe("a kind's own pane", () => {
  it("sources every row by its plugin and carries the plugin's update state", () => {
    const rows = kindRows(
      read([components({ skills: ['one'], available_version: '1.1.0' })]),
      'skills',
    );
    expect(rows).toHaveLength(1);
    expect(rows[0]?.name).toBe('one');
    expect(rows[0]?.source).toBe('demo');
    expect(rows[0]?.sourceId).toBe('demo@probe');
    expect(rows[0]?.stale).toBe(true);
  });

  it('keys an LSP check by plugin AND server, and carries it into the fold', () => {
    const entry = components({
      plugin: 'lsp-plugin@probe',
      lsp_servers: { alpha: 'alpha-bin', beta: 'beta-bin' },
    });
    const wire = read([entry], {
      lsp: [
        { plugin: 'lsp-plugin@probe', server: 'alpha', command: 'alpha-bin', on_path: true },
        { plugin: 'lsp-plugin@probe', server: 'beta', command: 'beta-bin', on_path: false },
      ],
    });

    const rows = kindRows(wire, 'lsp');
    expect(rows.map((row) => `${row.name}:${row.note}`)).toEqual([
      'alpha:alpha: on PATH',
      'beta:beta: missing',
    ]);

    const groups = foldGroups(entry, wire.lsp);
    const lsp = groups.find((group) => group.kind === 'lsp');
    expect(lsp?.rows.map((row) => `${row.name}:${row.note}`)).toEqual([
      'alpha:alpha: on PATH',
      'beta:beta: missing',
    ]);
  });

  it("reads an LSP row's note off the server's PATH check", () => {
    const wire = read(
      [
        components({
          plugin: 'rust-analyzer-lsp@probe',
          lsp_servers: { 'rust-analyzer': 'rust-analyzer' },
        }),
      ],
      {
        lsp: [
          {
            plugin: 'rust-analyzer-lsp@probe',
            server: 'rust-analyzer',
            command: 'rust-analyzer',
            on_path: true,
          },
        ],
      },
    );
    expect(kindRows(wire, 'lsp')[0]?.note).toBe('rust-analyzer: on PATH');
    expect(kindRows(read(wire.snapshot?.components ?? [], { lsp: [] }), 'lsp')[0]?.note).toBeNull();
  });

  it('drops rows of uninstalled plugins from a kind pane', () => {
    expect(kindRows(read([components({ installed: false, skills: ['gone'] })]), 'skills')).toEqual(
      [],
    );
  });
});

describe('the marketplace section', () => {
  it('lists the plugins that are not installed here', () => {
    const rows = availableRows(
      read([components({ plugin: 'new@probe', installed: false, version: null, skills: ['s'] })]),
    );
    expect(rows).toHaveLength(1);
    expect(rows[0]?.id).toBe('new@probe');
    expect(rows[0]?.detail).toBe('1 skill');
  });

  it('joins a marketplace with its health, and names drift and load failures', () => {
    const rows = marketplaceRows(
      read([], {
        marketplaces: [
          { name: 'ok', source: 'github', repo: 'owner/ok', install_location: null },
          { name: 'drifted', source: 'directory', repo: null, install_location: '/tmp/clone' },
          { name: 'broken', source: 'github', repo: 'owner/broken', install_location: null },
        ],
        health: [
          { name: 'ok', source: 'github', available: 37, load_error: null, drifted: false },
          { name: 'drifted', source: 'directory', available: 1, load_error: null, drifted: true },
          {
            name: 'broken',
            source: 'github',
            available: 0,
            load_error: 'marketplace.json failed to parse',
            drifted: false,
          },
        ],
      }),
    );
    const [ok, drifted, broken] = rows;
    if (ok === undefined || drifted === undefined || broken === undefined) {
      throw new Error('the three marketplaces did not draw');
    }
    expect(ok.problem).toBeNull();
    expect(drifted.drifted).toBe(true);
    expect(drifted.problem).toContain('registry drift');
    expect(broken.loadFailed).toBe(true);
    expect(broken.problem).toBe('marketplace.json failed to parse');
    expect(marketplaceDetail(ok)).toBe('owner/ok · 37 plugins');
    expect(marketplaceDetail(drifted)).toBe('/tmp/clone · 1 plugin');
  });
});
