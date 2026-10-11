/**
 * The wire the extensions page's tests draw: a stale plugin, a current
 * one, a not-installed one and a drifted marketplace, which together put
 * every row kind the page has in front of whatever is checking.
 */

import type { Command } from '../protocol';
import type { ProjectView } from '../wire/home';
import type { ExtensionsRead, PluginComponents } from '../wire/extensions';

export function components(over: Partial<PluginComponents> = {}): PluginComponents {
  return {
    plugin: 'superpowers@claude-plugins-official',
    marketplace: 'claude-plugins-official',
    version: '6.4.1',
    installed: true,
    enabled: true,
    auto: false,
    available_version: '6.5.0',
    skills: ['brainstorming', 'writing-plans'],
    agents: [],
    commands: [],
    hooks: ['SessionStart'],
    mcp: false,
    lsp_servers: {},
    load_error: null,
    ...over,
  };
}

export function extensionsWire(over: Partial<ExtensionsRead> = {}): ExtensionsRead {
  return {
    snapshot: {
      // One entry in a non-user scope, so the scope join has something to
      // carry: an Update that hardcodes `user` cannot move this plugin.
      installed: [
        {
          id: 'pensive@claude-night-market',
          version: '1.9.21',
          scope: 'local',
          enabled: true,
          project_path: '/tmp/proj',
        },
      ],
      marketplace: [],
      marketplaces: [
        {
          name: 'claude-plugins-official',
          source: 'github',
          repo: 'anthropics/claude-plugins-official',
          install_location: null,
        },
        { name: 'stx-clarity', source: 'directory', repo: null, install_location: '/tmp/clone' },
      ],
      components: [
        components(),
        components({
          plugin: 'pensive@claude-night-market',
          marketplace: 'claude-night-market',
          version: '1.9.21',
          available_version: '1.9.22',
          skills: ['architecture-review'],
          hooks: [],
        }),
        components({
          plugin: 'typescript-lsp@claude-plugins-official',
          marketplace: 'claude-plugins-official',
          version: '1.0.0',
          available_version: null,
          installed: false,
          enabled: false,
          skills: [],
          lsp_servers: { 'typescript-language-server': 'typescript-language-server' },
        }),
      ],
      marketplace_health: [
        {
          name: 'claude-plugins-official',
          source: 'github',
          available: 37,
          load_error: null,
          drifted: false,
        },
        { name: 'stx-clarity', source: 'directory', available: 1, load_error: null, drifted: true },
      ],
      ...(over.snapshot ?? {}),
    },
    update_run: null,
    lsp: [],
    ...over,
  };
}

export const project: ProjectView = {
  key: '/tmp/proj',
  name: 'proj',
  org: 'TestOrg',
  path: '/tmp/proj',
  display_path: '/tmp/proj',
  accounts: [],
  fallback_accounts: [],
  has_model: true,
  sessions: [],
};

/** The page's action props, each recording what it was handed. */
export interface Recorder {
  commands: Command[];
  actions: {
    update: (pluginId: string, scope: string) => void;
    updateall: () => void;
    check: () => void;
    install: (pluginId: string, scope: string, project: string | null) => void;
    repair: (name: string) => void;
    addmarketplace: (source: string) => void;
  };
}

export function recorder(): Recorder {
  const commands: Command[] = [];
  return {
    commands,
    actions: {
      update: (pluginId: string, scope: string) =>
        commands.push({ plugin_update: { plugin_id: pluginId, scope } }),
      updateall: () => commands.push('plugin_update_all'),
      check: () => commands.push('plugin_check_updates'),
      install: (pluginId: string, scope: string, project: string | null) =>
        commands.push({ plugin_install: { plugin_id: pluginId, scope, project } }),
      repair: (name: string) => commands.push({ marketplace_repair: { name } }),
      addmarketplace: (source: string) => commands.push({ marketplace_add: { source } }),
    },
  };
}
